//! Handler for `POST /ai/multi-agent` (and `/ai/passive-suggestions`).
//!
//! The Warp client sends a **protobuf** [`warp_multi_agent_api::Request`] body
//! and expects an **SSE** stream where each `data:` line is a
//! **base64-url-safe-encoded** protobuf [`warp_multi_agent_api::ResponseEvent`].
//!
//! Minimum viable event sequence the client needs:
//! 1. `Init { conversation_id, request_id, run_id }` — stream start marker
//! 2. One or more `ClientActions` containing:
//!    - `CreateTask` (once, to establish the task/message container)
//!    - `AddMessagesToTask` with a `UserQuery` message (echo the user's input)
//!    - `AddMessagesToTask` with an `AgentOutput` message (the assistant reply)
//!    - `AppendToMessageContent` chunks (for streaming text)
//! 3. `Finished { reason: Done }` — stream end marker

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use prost::Message;

use crate::server::AppState;

/// Extract user query text from the decoded protobuf Request.
fn extract_user_query(request: &warp_multi_agent_api::Request) -> Option<String> {
    let input = request.input.as_ref()?;
    let input_type = input.r#type.as_ref()?;

    match input_type {
        // Modern path: UserInputs with repeated UserInput
        warp_multi_agent_api::request::input::Type::UserInputs(user_inputs) => {
            for ui in &user_inputs.inputs {
                if let Some(input_oneof) = &ui.input {
                    use warp_multi_agent_api::request::input::user_inputs::user_input::Input;
                    match input_oneof {
                        Input::UserQuery(q) => return Some(q.query.clone()),
                        Input::CliAgentUserQuery(q) => return Some(q.user_query.as_ref()?.query.clone()),
                        _ => {}
                    }
                }
            }
            None
        }
        // Deprecated but still used path
        warp_multi_agent_api::request::input::Type::UserQuery(q) => Some(q.query.clone()),
        _ => None,
    }
}

/// Encode a `ResponseEvent` as an SSE `data:` line.
fn sse_line(event: &warp_multi_agent_api::ResponseEvent) -> String {
    let bytes = event.encode_to_vec();
    let b64 = URL_SAFE.encode(&bytes);
    format!("data: \"{b64}\"\n\n")
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Decode the protobuf request
    let request = match warp_multi_agent_api::Request::decode(body.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to decode multi-agent Request protobuf: {e}");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(format!("Invalid protobuf: {e}")))
                .unwrap();
        }
    };

    let user_query = extract_user_query(&request).unwrap_or_default();
    tracing::info!(query = %user_query, "multi-agent request");

    if user_query.is_empty() {
        tracing::warn!("multi-agent request with empty user query, sending done");
    }

    // Generate IDs
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    let task_id = uuid::Uuid::new_v4().to_string();
    let user_msg_id = uuid::Uuid::new_v4().to_string();
    let assistant_msg_id = uuid::Uuid::new_v4().to_string();

    // Call the OpenAI-compatible backend
    let ai_response = call_backend(&state, &user_query).await;

    let assistant_text = match ai_response {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("Backend call failed: {e}");
            format!("Error calling AI backend: {e}")
        }
    };

    // Build the SSE event sequence
    let mut sse_body = String::new();

    // 1. StreamInit
    let init_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::Init(
            warp_multi_agent_api::response_event::StreamInit {
                conversation_id: conversation_id.clone(),
                request_id: request_id.clone(),
                run_id: run_id.clone(),
            },
        )),
    };
    sse_body.push_str(&sse_line(&init_event));

    // 2. CreateTask
    let create_task_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(warp_multi_agent_api::client_action::Action::CreateTask(
                        warp_multi_agent_api::client_action::CreateTask {
                            task: Some(warp_multi_agent_api::Task {
                                id: task_id.clone(),
                                ..Default::default()
                            }),
                        },
                    )),
                }],
            },
        )),
    };
    sse_body.push_str(&sse_line(&create_task_event));

    // 3. AddMessagesToTask — user query echo
    let user_message = warp_multi_agent_api::Message {
        id: user_msg_id.clone(),
        task_id: task_id.clone(),
        request_id: request_id.clone(),
        message: Some(warp_multi_agent_api::message::Message::UserQuery(
            warp_multi_agent_api::message::UserQuery {
                query: user_query.clone(),
                ..Default::default()
            },
        )),
        ..Default::default()
    };

    let add_user_msg_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.clone(),
                                messages: vec![user_message],
                            },
                        ),
                    ),
                }],
            },
        )),
    };
    sse_body.push_str(&sse_line(&add_user_msg_event));

    // 4. AddMessagesToTask — assistant output (initial empty, then append)
    let assistant_message = warp_multi_agent_api::Message {
        id: assistant_msg_id.clone(),
        task_id: task_id.clone(),
        request_id: request_id.clone(),
        message: Some(warp_multi_agent_api::message::Message::AgentOutput(
            warp_multi_agent_api::message::AgentOutput {
                text: String::new(),
            },
        )),
        ..Default::default()
    };

    let add_assistant_msg_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.clone(),
                                messages: vec![assistant_message],
                            },
                        ),
                    ),
                }],
            },
        )),
    };
    sse_body.push_str(&sse_line(&add_assistant_msg_event));

    // 5. AppendToMessageContent — the actual assistant text
    let append_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AppendToMessageContent(
                            warp_multi_agent_api::client_action::AppendToMessageContent {
                                task_id: task_id.clone(),
                                message: Some(warp_multi_agent_api::Message {
                                    id: assistant_msg_id.clone(),
                                    task_id: task_id.clone(),
                                    request_id: request_id.clone(),
                                    message: Some(
                                        warp_multi_agent_api::message::Message::AgentOutput(
                                            warp_multi_agent_api::message::AgentOutput {
                                                text: assistant_text,
                                            },
                                        ),
                                    ),
                                    ..Default::default()
                                }),
                                mask: Some(prost_types::FieldMask {
                                    paths: vec!["agent_output.text".to_string()],
                                }),
                            },
                        ),
                    ),
                }],
            },
        )),
    };
    sse_body.push_str(&sse_line(&append_event));

    // 6. StreamFinished
    let finished_event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::Finished(
            warp_multi_agent_api::response_event::StreamFinished {
                reason: Some(
                    warp_multi_agent_api::response_event::stream_finished::Reason::Done(
                        warp_multi_agent_api::response_event::stream_finished::Done {},
                    ),
                ),
                ..Default::default()
            },
        )),
    };
    sse_body.push_str(&sse_line(&finished_event));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(sse_body))
        .unwrap()
}

/// Call the OpenAI-compatible backend with the user's query.
async fn call_backend(
    state: &AppState,
    user_query: &str,
) -> Result<String, anyhow::Error> {
    let url = state.config.chat_completions_url();
    let model = &state.config.default_model;

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a helpful AI assistant integrated into the Warp terminal. \
                            Respond concisely and helpfully. When providing code, use markdown \
                            code blocks with language annotations."
            },
            {
                "role": "user",
                "content": user_query
            }
        ],
        "max_tokens": 4096,
        "stream": false
    });

    let resp = state
        .http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Backend returned {status}: {body}");
    }

    let json: serde_json::Value = resp.json().await?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("(no response from model)")
        .to_string();

    Ok(text)
}
