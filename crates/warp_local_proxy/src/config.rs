//! Runtime configuration for the proxy. Built from CLI args (with env var
//! fallbacks) so the operator can wire it up via shell rc or a systemd unit
//! without an extra config file.

use std::net::SocketAddr;

use clap::{Parser, ValueEnum};

/// How the proxy authenticates to the backend AI server. Each style affects
/// both the auth header shape and (for Azure) the URL composition.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`. Works for OpenAI, GitHub Copilot, most
    /// OpenAI-compatible local servers, and the user's `localhost:3113`.
    Bearer,
    /// `api-key: <key>`. Required by Azure OpenAI and Azure AI Foundry's
    /// legacy completions endpoints. When this is selected and `--api-version`
    /// is set, the proxy appends `?api-version=<version>` to backend requests.
    AzureApiKey,
    /// No auth header at all. Use for Ollama / LM Studio / vLLM in dev mode.
    None,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "warp-local-proxy",
    version,
    about = "Local proxy for the Warp client"
)]
pub struct Config {
    /// Address to bind the HTTP server to. Defaults to localhost-only.
    #[arg(long, env = "WARP_LOCAL_PROXY_BIND", default_value = "127.0.0.1:8765")]
    pub bind: SocketAddr,

    /// Base URL of the AI backend. The proxy appends `/chat/completions` and
    /// `/models` to this base.
    ///
    /// Examples:
    ///   * Bearer-style local proxies: `http://localhost:3113/v1`
    ///     (the user's gateway), `http://localhost:8000/v1`
    ///   * Ollama: `http://localhost:11434/v1`
    ///   * LM Studio: `http://localhost:1234/v1`
    ///   * OpenAI direct: `https://api.openai.com/v1`
    ///   * Azure OpenAI: `https://<resource>.openai.azure.com/openai/deployments/<deployment>`
    ///   * Azure AI Foundry: `https://<endpoint>.services.ai.azure.com/openai/v1`
    #[arg(
        long,
        env = "WARP_LOCAL_PROXY_BACKEND",
        default_value = "http://localhost:3113/v1"
    )]
    pub backend_base_url: String,

    /// Auth style for the backend. See [`AuthStyle`] for the options.
    #[arg(
        long,
        value_enum,
        env = "WARP_LOCAL_PROXY_AUTH_STYLE",
        default_value_t = AuthStyle::Bearer
    )]
    pub backend_auth_style: AuthStyle,

    /// API key sent to the backend (as `Authorization: Bearer <key>` for
    /// `bearer`, or `api-key: <key>` for `azure-api-key`). Most local backends
    /// (Ollama, LM Studio) ignore this; OpenAI / Azure / hosted gateways
    /// require it.
    #[arg(long, env = "WARP_LOCAL_PROXY_BACKEND_API_KEY")]
    pub backend_api_key: Option<String>,

    /// Azure-only: the `api-version` query parameter to append to backend
    /// requests (e.g. `2024-02-15-preview`, `2024-08-01-preview`,
    /// `2025-04-01-preview`). Ignored unless `--backend-auth-style` is
    /// `azure-api-key`.
    #[arg(long, env = "WARP_LOCAL_PROXY_AZURE_API_VERSION")]
    pub azure_api_version: Option<String>,

    /// Default model id to use for AI calls when the operation does not name
    /// a specific model. Must match an id the backend exposes via `/v1/models`
    /// (or, for Azure, the deployment name baked into the URL).
    #[arg(
        long,
        env = "WARP_LOCAL_PROXY_DEFAULT_MODEL",
        default_value = "gpt-5-mini"
    )]
    pub default_model: String,
}

impl Config {
    pub fn from_cli() -> Self {
        Self::parse()
    }

    /// Builds the URL the proxy will POST chat completions to, including the
    /// Azure `api-version` query string when configured.
    pub fn chat_completions_url(&self) -> String {
        let trimmed = self.backend_base_url.trim_end_matches('/');
        let mut url = format!("{trimmed}/chat/completions");
        if matches!(self.backend_auth_style, AuthStyle::AzureApiKey) {
            if let Some(v) = self.azure_api_version.as_deref().filter(|s| !s.is_empty()) {
                url.push_str("?api-version=");
                url.push_str(v);
            }
        }
        url
    }

    /// Builds the URL for listing models (if the backend supports it).
    /// Returns `None` for Azure OpenAI where the deployments API requires
    /// management-level auth not available with just an API key.
    pub fn models_url(&self) -> Option<String> {
        if matches!(self.backend_auth_style, AuthStyle::AzureApiKey) {
            // Azure OpenAI's /openai/deployments requires management auth,
            // and /openai/models returns the full catalog (300+ models).
            // Neither is useful with just an api-key. Return None so the
            // proxy uses the configured default_model directly.
            None
        } else {
            let trimmed = self.backend_base_url.trim_end_matches('/');
            Some(format!("{trimmed}/models"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: &str, style: AuthStyle, version: Option<&str>) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            backend_base_url: base.into(),
            backend_auth_style: style,
            backend_api_key: None,
            azure_api_version: version.map(String::from),
            default_model: "gpt-5-mini".into(),
        }
    }

    #[test]
    fn bearer_chat_url_simple() {
        let c = cfg("http://localhost:3113/v1", AuthStyle::Bearer, None);
        assert_eq!(
            c.chat_completions_url(),
            "http://localhost:3113/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_is_normalised() {
        let c = cfg("http://localhost:3113/v1/", AuthStyle::Bearer, None);
        assert_eq!(
            c.chat_completions_url(),
            "http://localhost:3113/v1/chat/completions"
        );
    }

    #[test]
    fn azure_appends_api_version() {
        let c = cfg(
            "https://foo.openai.azure.com/openai/deployments/gpt5",
            AuthStyle::AzureApiKey,
            Some("2024-08-01-preview"),
        );
        assert_eq!(
            c.chat_completions_url(),
            "https://foo.openai.azure.com/openai/deployments/gpt5/chat/completions?api-version=2024-08-01-preview"
        );
    }

    #[test]
    fn bearer_ignores_api_version_even_if_set() {
        let c = cfg(
            "http://localhost:3113/v1",
            AuthStyle::Bearer,
            Some("2024-08-01-preview"),
        );
        assert_eq!(
            c.chat_completions_url(),
            "http://localhost:3113/v1/chat/completions",
            "api-version is Azure-only"
        );
    }

    #[test]
    fn azure_without_api_version_omits_query() {
        let c = cfg(
            "https://foo.openai.azure.com/openai/deployments/gpt5",
            AuthStyle::AzureApiKey,
            None,
        );
        assert_eq!(
            c.chat_completions_url(),
            "https://foo.openai.azure.com/openai/deployments/gpt5/chat/completions"
        );
    }

    #[test]
    fn none_auth_no_query() {
        let c = cfg(
            "http://localhost:11434/v1",
            AuthStyle::None,
            Some("ignored"),
        );
        assert_eq!(
            c.chat_completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_mirrors_chat_url() {
        let c = cfg("http://localhost:3113/v1", AuthStyle::Bearer, None);
        assert_eq!(c.models_url(), Some("http://localhost:3113/v1/models".into()));
    }

    #[test]
    fn models_url_none_for_azure() {
        let c = cfg(
            "https://foo.openai.azure.com/openai/deployments/gpt-4o",
            AuthStyle::AzureApiKey,
            Some("2024-10-21"),
        );
        assert_eq!(c.models_url(), None);
    }
}
