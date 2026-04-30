pub use crate::aws_credentials::{AwsCredentials, AwsCredentialsState};
use serde::{Deserialize, Serialize};
use warp_multi_agent_api as api;
use warpui::{Entity, ModelContext, SingletonEntity};
use warpui_extras::secure_storage::{self, AppContextExt};

const SECURE_STORAGE_KEY: &str = "AiApiKeys";

/// Emitted when user-provided API keys are updated in-memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeyManagerEvent {
    KeysUpdated,
}

/// User-provided API keys for AI providers.
///
/// These are used for "Bring Your Own API Key" functionality, allowing
/// users to use their own API keys instead of Warp's.
///
/// **[FORK]** The optional `*_base_url` fields let a `LocalAiClient` (gated
/// by `FeatureFlag::LocalModels`) target an OpenAI-compatible endpoint such
/// as Ollama, LM Studio, vLLM, Azure OpenAI, or a self-hosted proxy. When the
/// URL is `None`, requests go to the canonical provider host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiKeys {
    pub google: Option<String>,
    pub anthropic: Option<String>,
    pub openai: Option<String>,
    pub open_router: Option<String>,

    /// [FORK] Optional override for the OpenAI base URL (e.g.
    /// `http://localhost:11434/v1` for Ollama, `http://localhost:1234/v1`
    /// for LM Studio, or an Azure OpenAI deployment URL). `None` means use
    /// `https://api.openai.com/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_base_url: Option<String>,

    /// [FORK] Optional override for the Anthropic base URL (proxies, internal
    /// gateways). `None` means use `https://api.anthropic.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_base_url: Option<String>,
}

impl ApiKeys {
    pub fn has_any_key(&self) -> bool {
        self.openai.is_some()
            || self.anthropic.is_some()
            || self.google.is_some()
            || self.open_router.is_some()
    }

    /// [FORK] Effective OpenAI base URL — user override if set, otherwise the
    /// canonical OpenAI host. Always includes the `/v1` path segment so
    /// callers can append `/chat/completions` etc. directly.
    pub fn effective_openai_base_url(&self) -> &str {
        self.openai_base_url
            .as_deref()
            .unwrap_or("https://api.openai.com/v1")
    }

    /// [FORK] Effective Anthropic base URL — user override if set, otherwise
    /// the canonical Anthropic host (no path suffix; callers append `/v1/...`).
    pub fn effective_anthropic_base_url(&self) -> &str {
        self.anthropic_base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
    }
}

/// Controls how AWS credentials are refreshed by [`ApiKeyManager`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AwsCredentialsRefreshStrategy {
    /// Load credentials from the local AWS credential chain (~/.aws). This is the default.
    #[default]
    LocalChain,
    /// Credentials are managed externally via OIDC/STS.
    /// The task ID is used to scope the STS AssumeRoleWithWebIdentity session.
    /// The role ARN is the IAM role to assume via STS.
    OidcManaged {
        task_id: Option<String>,
        role_arn: String,
    },
}

/// A structure that manages API keys for AI providers.
pub struct ApiKeyManager {
    keys: ApiKeys,
    pub(crate) aws_credentials_state: AwsCredentialsState,
    aws_credentials_refresh_strategy: AwsCredentialsRefreshStrategy,
}

impl ApiKeyManager {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let keys = Self::load_keys_from_secure_storage(ctx);
        Self {
            keys,
            aws_credentials_state: AwsCredentialsState::Missing,
            aws_credentials_refresh_strategy: AwsCredentialsRefreshStrategy::default(),
        }
    }

    pub fn keys(&self) -> &ApiKeys {
        &self.keys
    }

    pub fn set_google_key(&mut self, key: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.google = key;
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    pub fn set_anthropic_key(&mut self, key: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.anthropic = key;
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    pub fn set_openai_key(&mut self, key: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.openai = key;
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    pub fn set_open_router_key(&mut self, key: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.open_router = key;
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    /// [FORK] Sets the OpenAI base URL override (e.g. for Ollama / LM Studio /
    /// Azure OpenAI). Pass `None` to revert to the canonical host.
    pub fn set_openai_base_url(&mut self, url: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.openai_base_url = url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    /// [FORK] Sets the Anthropic base URL override. Pass `None` to revert to
    /// the canonical host.
    pub fn set_anthropic_base_url(&mut self, url: Option<String>, ctx: &mut ModelContext<Self>) {
        self.keys.anthropic_base_url = url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
        self.write_keys_to_secure_storage(ctx);
    }

    pub fn set_aws_credentials_state(
        &mut self,
        state: AwsCredentialsState,
        ctx: &mut ModelContext<Self>,
    ) {
        self.aws_credentials_state = state;
        ctx.emit(ApiKeyManagerEvent::KeysUpdated);
    }

    pub fn aws_credentials_state(&self) -> &AwsCredentialsState {
        &self.aws_credentials_state
    }

    pub fn aws_credentials_refresh_strategy(&self) -> AwsCredentialsRefreshStrategy {
        self.aws_credentials_refresh_strategy.clone()
    }

    pub fn set_aws_credentials_refresh_strategy(
        &mut self,
        strategy: AwsCredentialsRefreshStrategy,
    ) {
        self.aws_credentials_refresh_strategy = strategy;
    }

    pub fn api_keys_for_request(
        &self,
        include_byo_keys: bool,
        include_aws_bedrock_credentials: bool,
    ) -> Option<api::request::settings::ApiKeys> {
        let anthropic = include_byo_keys
            .then(|| self.keys.anthropic.clone())
            .flatten()
            .unwrap_or_default();
        let openai = include_byo_keys
            .then(|| self.keys.openai.clone())
            .flatten()
            .unwrap_or_default();
        let google = include_byo_keys
            .then(|| self.keys.google.clone())
            .flatten()
            .unwrap_or_default();
        let open_router = include_byo_keys
            .then(|| self.keys.open_router.clone())
            .flatten()
            .unwrap_or_default();
        // Also include credentials when running with OIDC-managed Bedrock inference, regardless
        // of the per-user setting flag (which only applies to the local credential chain path).
        let include_aws = include_aws_bedrock_credentials
            || matches!(
                self.aws_credentials_refresh_strategy,
                AwsCredentialsRefreshStrategy::OidcManaged { .. }
            );
        let aws_credentials = include_aws
            .then(|| match self.aws_credentials_state {
                AwsCredentialsState::Loaded {
                    ref credentials, ..
                } => Some(credentials.clone().into()),
                _ => None,
            })
            .flatten();

        if anthropic.is_empty()
            && openai.is_empty()
            && google.is_empty()
            && open_router.is_empty()
            && aws_credentials.is_none()
        {
            None
        } else {
            Some(api::request::settings::ApiKeys {
                anthropic,
                openai,
                google,
                open_router,
                allow_use_of_warp_credits: false,
                aws_credentials,
            })
        }
    }

    fn load_keys_from_secure_storage(ctx: &mut ModelContext<Self>) -> ApiKeys {
        let key_json = match ctx.secure_storage().read_value(SECURE_STORAGE_KEY) {
            Ok(json) => json,
            Err(e) => {
                if !matches!(e, secure_storage::Error::NotFound) {
                    log::error!("Failed to read API keys from secure storage: {e:#}");
                }
                return ApiKeys::default();
            }
        };

        let keys = match serde_json::from_str(&key_json) {
            Ok(keys) => keys,
            Err(e) => {
                log::error!("Failed to deserialize API keys: {e:#}");
                ApiKeys::default()
            }
        };

        keys
    }

    fn write_keys_to_secure_storage(&mut self, ctx: &mut ModelContext<Self>) {
        let keys = self.keys.clone();

        let json = match serde_json::to_string(&keys) {
            Ok(json) => json,
            Err(e) => {
                log::error!("Failed to serialize API keys: {e:#}");
                return;
            }
        };

        if let Err(e) = ctx.secure_storage().write_value(SECURE_STORAGE_KEY, &json) {
            log::error!("Failed to write API keys to secure storage: {e:#}");
        }
    }
}

impl Entity for ApiKeyManager {
    type Event = ApiKeyManagerEvent;
}

impl SingletonEntity for ApiKeyManager {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_none() {
        let keys = ApiKeys::default();
        assert!(keys.openai_base_url.is_none());
        assert!(keys.anthropic_base_url.is_none());
        assert!(!keys.has_any_key());
    }

    #[test]
    fn effective_openai_base_url_falls_back_to_canonical() {
        let keys = ApiKeys::default();
        assert_eq!(keys.effective_openai_base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn effective_openai_base_url_uses_override() {
        let keys = ApiKeys {
            openai_base_url: Some("http://localhost:11434/v1".into()),
            ..Default::default()
        };
        assert_eq!(keys.effective_openai_base_url(), "http://localhost:11434/v1");
    }

    #[test]
    fn effective_anthropic_base_url_falls_back_to_canonical() {
        let keys = ApiKeys::default();
        assert_eq!(
            keys.effective_anthropic_base_url(),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn effective_anthropic_base_url_uses_override() {
        let keys = ApiKeys {
            anthropic_base_url: Some("https://my-proxy.example.com".into()),
            ..Default::default()
        };
        assert_eq!(
            keys.effective_anthropic_base_url(),
            "https://my-proxy.example.com"
        );
    }

    /// Old persisted ApiKeys JSON (pre-fork shape) must still deserialize so
    /// upgrades don't lose user keys from secure storage.
    #[test]
    fn deserializes_legacy_json_without_url_fields() {
        let legacy = r#"{
            "google": null,
            "anthropic": "ant-key",
            "openai": "oai-key",
            "open_router": null
        }"#;
        let keys: ApiKeys = serde_json::from_str(legacy).expect("legacy json must deserialize");
        assert_eq!(keys.openai.as_deref(), Some("oai-key"));
        assert_eq!(keys.anthropic.as_deref(), Some("ant-key"));
        assert!(keys.openai_base_url.is_none());
        assert!(keys.anthropic_base_url.is_none());
    }

    /// When neither URL is set, the new fields are omitted from JSON so we
    /// don't pollute persisted blobs with `null`s.
    #[test]
    fn serialization_omits_unset_url_fields() {
        let keys = ApiKeys::default();
        let json = serde_json::to_string(&keys).unwrap();
        assert!(!json.contains("openai_base_url"), "got {json}");
        assert!(!json.contains("anthropic_base_url"), "got {json}");
    }

    #[test]
    fn serialization_includes_set_url_fields() {
        let keys = ApiKeys {
            openai_base_url: Some("http://localhost:11434/v1".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&keys).unwrap();
        assert!(json.contains("openai_base_url"));
        assert!(json.contains("11434"));
    }
}
