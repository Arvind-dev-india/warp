//! Real AI handlers: `generateCommands`, `generateDialogue`. These call the
//! configured backend (OpenAI-compatible) and translate the response into the
//! GraphQL shape the cynic-typed client expects.

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::upstream::openai::{chat_completion, ChatMessage};

const COMMANDS_SYSTEM_PROMPT: &str = "\
You are a CLI command generator for a terminal app. Given a natural-language \
prompt, return ONLY a JSON object with this exact shape:

{
  \"commands\": [
    {
      \"command\": \"<shell command>\",
      \"description\": \"<one-line description>\",
      \"parameters\": [
        {\"id\": \"<param-id>\", \"description\": \"<what this param is>\"}
      ]
    }
  ]
}

Rules:
- Return at most 3 commands, ordered by likelihood.
- Use `{{param-id}}` placeholders inside `command` where appropriate. Each \
  placeholder MUST appear in the corresponding `parameters` array.
- If the prompt is too vague to generate any command, return an empty \
  `commands` array.
- DO NOT include markdown, prose, code fences, or any text outside the JSON.";

const DIALOGUE_SYSTEM_PROMPT: &str = "\
You are an assistant inside the Warp terminal. Answer the user's question \
concisely and accurately. If shell commands are involved, format them in \
code blocks. Avoid filler.";

#[derive(Debug, Deserialize)]
struct GeneratedCommandsBody {
    commands: Vec<GeneratedCommand>,
}

#[derive(Debug, Deserialize)]
struct GeneratedCommand {
    command: String,
    description: String,
    #[serde(default)]
    parameters: Vec<GeneratedCommandParameter>,
}

#[derive(Debug, Deserialize)]
struct GeneratedCommandParameter {
    id: String,
    description: String,
}

/// Implements the `generateCommands` mutation by calling the backend in JSON
/// mode and reformatting the response into the cynic-expected GraphQL shape.
pub async fn generate_commands(
    http: &reqwest::Client,
    config: &Config,
    variables: &Value,
) -> Value {
    let prompt = variables
        .get("input")
        .and_then(|i| i.get("prompt"))
        .and_then(|p| p.as_str())
        .unwrap_or("");

    let messages = vec![
        ChatMessage {
            role: "system",
            content: COMMANDS_SYSTEM_PROMPT.into(),
        },
        ChatMessage {
            role: "user",
            content: prompt.to_string(),
        },
    ];

    match chat_completion(http, config, messages, true, Some(800)).await {
        Ok(text) => match parse_commands_json(&text) {
            Ok(commands) => commands_success(commands),
            Err(err) => {
                tracing::warn!(?err, raw = %text, "model returned malformed JSON for generateCommands");
                commands_failure("BAD_PROMPT")
            }
        },
        Err(err) => {
            tracing::error!(?err, "backend chat completion failed for generateCommands");
            commands_failure("AI_PROVIDER_ERROR")
        }
    }
}

fn parse_commands_json(text: &str) -> anyhow::Result<Vec<GeneratedCommand>> {
    // Accept either a bare {"commands":[...]} body or one with surrounding
    // JSON noise (some local models add a brief preamble).
    let trimmed = text.trim();
    let json_start = trimmed.find('{').context("no JSON object in response")?;
    let json_text = &trimmed[json_start..];
    let body: GeneratedCommandsBody = serde_json::from_str(json_text)
        .or_else(|_| {
            // Try truncating at the last closing brace to handle tail noise.
            let end = json_text
                .rfind('}')
                .context("no closing brace in response")?;
            let trimmed = &json_text[..=end];
            serde_json::from_str::<GeneratedCommandsBody>(trimmed).map_err(anyhow::Error::from)
        })
        .context("response did not match {commands: [...]} shape")?;
    Ok(body.commands)
}

fn commands_success(commands: Vec<GeneratedCommand>) -> Value {
    let cmds: Vec<Value> = commands
        .into_iter()
        .map(|c| {
            let params: Vec<Value> = c
                .parameters
                .into_iter()
                .map(|p| json!({ "id": p.id, "description": p.description }))
                .collect();
            json!({
                "command": c.command,
                "description": c.description,
                "parameters": params,
            })
        })
        .collect();
    json!({
        "generateCommands": {
            "__typename": "GenerateCommandsOutput",
            "status": {
                "__typename": "GenerateCommandsSuccess",
                "commands": cmds,
            },
            "responseContext": {"serverVersion": "warp_local_proxy/0.1.0"},
        }
    })
}

fn commands_failure(failure_type: &str) -> Value {
    json!({
        "generateCommands": {
            "__typename": "GenerateCommandsOutput",
            "status": {
                "__typename": "GenerateCommandsFailure",
                "type": failure_type,
            },
            "responseContext": {"serverVersion": "warp_local_proxy/0.1.0"},
        }
    })
}

/// Implements the `generateDialogue` mutation by calling the backend with the
/// transcript reformatted as alternating user/assistant chat messages.
pub async fn generate_dialogue(
    http: &reqwest::Client,
    config: &Config,
    variables: &Value,
) -> Value {
    let input = variables.get("input").cloned().unwrap_or_else(|| json!({}));
    let prompt = input
        .get("prompt")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let transcript = input
        .get("transcript")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let mut messages = vec![ChatMessage {
        role: "system",
        content: DIALOGUE_SYSTEM_PROMPT.into(),
    }];
    for part in &transcript {
        if let Some(user) = part.get("user").and_then(|v| v.as_str()) {
            if !user.is_empty() {
                messages.push(ChatMessage {
                    role: "user",
                    content: user.to_string(),
                });
            }
        }
        if let Some(assistant) = part.get("assistant").and_then(|v| v.as_str()) {
            if !assistant.is_empty() {
                messages.push(ChatMessage {
                    role: "assistant",
                    content: assistant.to_string(),
                });
            }
        }
    }
    messages.push(ChatMessage {
        role: "user",
        content: prompt,
    });

    match chat_completion(http, config, messages, false, Some(2048)).await {
        Ok(text) => dialogue_success(&text),
        Err(err) => {
            tracing::error!(?err, "backend chat completion failed for generateDialogue");
            dialogue_failure()
        }
    }
}

fn unlimited_request_limit_info() -> Value {
    json!({
        "isUnlimited": true,
        "nextRefreshTime": null,
        "requestLimit": 0,
        "requestsUsedSinceLastRefresh": 0
    })
}

fn dialogue_success(answer: &str) -> Value {
    json!({
        "generateDialogue": {
            "__typename": "GenerateDialogueOutput",
            "status": {
                "__typename": "GenerateDialogueSuccess",
                "answer": answer,
                "requestLimitInfo": unlimited_request_limit_info(),
                "transcriptSummarized": false,
                "truncated": false,
            },
            "responseContext": {"serverVersion": "warp_local_proxy/0.1.0"},
        }
    })
}

fn dialogue_failure() -> Value {
    json!({
        "generateDialogue": {
            "__typename": "GenerateDialogueOutput",
            "status": {
                "__typename": "GenerateDialogueFailure",
                "requestLimitInfo": unlimited_request_limit_info(),
            },
            "responseContext": {"serverVersion": "warp_local_proxy/0.1.0"},
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"commands":[{"command":"ls","description":"list files","parameters":[]}]}"#;
        let cmds = parse_commands_json(raw).unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "ls");
    }

    #[test]
    fn parses_json_with_preamble() {
        let raw = "Sure! Here you go:\n{\"commands\":[{\"command\":\"pwd\",\"description\":\"print directory\",\"parameters\":[]}]}";
        let cmds = parse_commands_json(raw).unwrap();
        assert_eq!(cmds[0].command, "pwd");
    }

    #[test]
    fn parses_json_with_tail_noise() {
        let raw = "{\"commands\":[{\"command\":\"id\",\"description\":\"print user id\",\"parameters\":[]}]}\n\nLet me know if you need more.";
        let cmds = parse_commands_json(raw).unwrap();
        assert_eq!(cmds[0].command, "id");
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_commands_json("just some prose").is_err());
    }
}
