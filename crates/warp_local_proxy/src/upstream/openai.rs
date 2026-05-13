//! Minimal OpenAI Chat Completions client used by the AI operation handlers.
//!
//! Speaks the OpenAI-compatible HTTP shape: `POST {base}/chat/completions` with
//! a JSON body and either `Authorization: Bearer <key>` (most backends) or
//! `api-key: <key>` (Azure OpenAI / Foundry). Returns the assistant's first
//! message content as a plain `String`.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use crate::config::{AuthStyle, Config};

#[derive(Debug, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatMessage<'a> {
    pub role: &'a str,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
pub struct ChatChoice {
    pub message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
pub struct ChatChoiceMessage {
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsListResponse {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Deserialize, Default)]
struct ModelCapabilities {
    #[serde(default)]
    pub chat_completion: bool,
    #[serde(default)]
    pub inference: bool,
}

/// Fetches the list of model ids the configured backend exposes via
/// `GET {base}/models`. Used at proxy startup to populate the local-mode
/// `FeatureModelChoice` so the Warp client's model picker shows real model
/// names instead of a single hardcoded entry.
///
/// Returns an empty Vec on any failure (network, auth, malformed body) — the
/// caller falls back to the single `LOCAL_FALLBACK_MODEL_ID` entry so the
/// client still gets a non-empty list.
pub async fn fetch_models(http: &reqwest::Client, config: &Config) -> Vec<String> {
    let url = match config.models_url() {
        Some(url) => url,
        None => {
            tracing::info!("no models endpoint; using configured default model only");
            return vec![config.default_model.clone()];
        }
    };
    let mut req = http.get(&url);
    match config.backend_auth_style {
        AuthStyle::Bearer => {
            if let Some(key) = config.backend_api_key.as_deref().filter(|s| !s.is_empty()) {
                req = req.bearer_auth(key);
            }
        }
        AuthStyle::AzureApiKey => {
            if let Some(key) = config.backend_api_key.as_deref().filter(|s| !s.is_empty()) {
                req = req.header("api-key", key);
            }
        }
        AuthStyle::None => {}
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<ModelsListResponse>().await {
            Ok(body) => {
                let models: Vec<String> = if matches!(config.backend_auth_style, AuthStyle::AzureApiKey) {
                    // Azure /openai/deployments returns your deployed models.
                    // Each entry has an "id" field = deployment name.
                    let mut ids: Vec<String> = body.data
                        .into_iter()
                        .map(|m| m.id)
                        .collect();
                    // Ensure default model is first
                    let default = &config.default_model;
                    if !ids.iter().any(|id| id == default) {
                        ids.insert(0, default.clone());
                    } else {
                        ids.retain(|id| id != default);
                        ids.insert(0, default.clone());
                    }
                    ids
                } else {
                    body.data.into_iter().map(|m| m.id).collect()
                };
                models
            }
            Err(err) => {
                tracing::warn!(?err, "backend /models response did not parse");
                Vec::new()
            }
        },
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "backend /models returned non-2xx");
            Vec::new()
        }
        Err(err) => {
            tracing::warn!(?err, "backend /models request failed");
            Vec::new()
        }
    }
}

/// Issues a chat completion against the configured backend and returns the
/// assistant's first message content. JSON-mode is requested via
/// `response_format` when `json_mode` is true; backends that don't support it
/// will simply ignore the field.
pub async fn chat_completion(
    http: &reqwest::Client,
    config: &Config,
    messages: Vec<ChatMessage<'_>>,
    json_mode: bool,
    max_tokens: Option<u32>,
) -> anyhow::Result<String> {
    let url = config.chat_completions_url();
    let body = ChatRequest {
        model: &config.default_model,
        messages,
        temperature: Some(0.2),
        max_tokens,
        response_format: if json_mode {
            Some(serde_json::json!({"type": "json_object"}))
        } else {
            None
        },
    };

    let mut req = http.post(&url).json(&body);
    match config.backend_auth_style {
        AuthStyle::Bearer => {
            if let Some(key) = config.backend_api_key.as_deref().filter(|s| !s.is_empty()) {
                req = req.bearer_auth(key);
            }
        }
        AuthStyle::AzureApiKey => {
            if let Some(key) = config.backend_api_key.as_deref().filter(|s| !s.is_empty()) {
                req = req.header("api-key", key);
            }
        }
        AuthStyle::None => {}
    }

    let resp = req.send().await.context("backend request failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "backend returned {status}: {body}",
            body = text.chars().take(800).collect::<String>()
        ));
    }

    let parsed: ChatResponse =
        serde_json::from_str(&text).context("failed to parse backend chat response")?;
    let content = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| anyhow!("backend returned no choices"))?;
    Ok(content)
}
