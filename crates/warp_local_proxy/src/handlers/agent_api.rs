use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{self as stream, StreamExt};

use crate::server::AppState;

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentRunEvent {
    pub event_type: String,
    pub run_id: String,
    pub ref_id: Option<String>,
    pub execution_id: Option<String>,
    pub occurred_at: String,
    pub sequence: i64,
}

#[derive(Clone, Debug, Serialize)]
struct StoredMessage {
    message_id: String,
    sender_run_id: String,
    subject: String,
    body: String,
    sent_at: String,
    delivered_at: Option<String>,
    read_at: Option<String>,
    recipients: Vec<String>,
}

pub struct AgentApiState {
    events: RwLock<Vec<AgentRunEvent>>,
    messages: RwLock<HashMap<String, StoredMessage>>,
    next_sequence: AtomicI64,
    event_sender: broadcast::Sender<AgentRunEvent>,
}

impl AgentApiState {
    pub fn new() -> Self {
        let (event_sender, _) = broadcast::channel(256);
        Self {
            events: RwLock::new(Vec::new()),
            messages: RwLock::new(HashMap::new()),
            next_sequence: AtomicI64::new(1),
            event_sender,
        }
    }

    fn emit(
        &self,
        run_id: String,
        event_type: String,
        ref_id: Option<String>,
        execution_id: Option<String>,
    ) -> AgentRunEvent {
        let event = AgentRunEvent {
            event_type,
            run_id,
            ref_id,
            execution_id,
            occurred_at: now_rfc3339(),
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
        };
        self.events
            .write()
            .expect("agent events lock poisoned")
            .push(event.clone());
        let _ = self.event_sender.send(event.clone());
        event
    }

    fn children(&self, run_id: &str) -> Vec<String> {
        self.events
            .read()
            .expect("agent events lock poisoned")
            .iter()
            .filter(|event| event.run_id == run_id && event.event_type == "child_agent_started")
            .filter_map(|event| event.ref_id.clone())
            .collect()
    }

    fn matches_ancestor(&self, ancestor_run_id: &str, include_self: bool, run_id: &str) -> bool {
        (include_self && run_id == ancestor_run_id)
            || self
                .children(ancestor_run_id)
                .iter()
                .any(|child| child == run_id)
    }

    pub(crate) fn store_message(
        &self,
        recipient_run_id: String,
        sender_run_id: String,
        subject: String,
        body: String,
    ) -> String {
        let message_id = uuid::Uuid::new_v4().to_string();
        let message = StoredMessage {
            message_id: message_id.clone(),
            sender_run_id,
            subject,
            body,
            sent_at: now_rfc3339(),
            delivered_at: None,
            read_at: None,
            recipients: vec![recipient_run_id.clone()],
        };
        self.messages
            .write()
            .expect("agent messages lock poisoned")
            .insert(message_id.clone(), message);
        self.emit(
            recipient_run_id,
            "new_message".to_string(),
            Some(message_id.clone()),
            None,
        );
        message_id
    }
}

impl Default for AgentApiState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default)]
struct EventFilter {
    run_ids: Vec<String>,
    ancestor_run_id: Option<String>,
    include_self: bool,
    since: i64,
}

impl EventFilter {
    fn from_query(raw_query: Option<&str>) -> Self {
        let mut filter = Self::default();
        for parameter in raw_query.unwrap_or_default().split('&') {
            let Some((key, value)) = parameter.split_once('=') else {
                continue;
            };
            match key {
                "run_ids[]" | "run_ids%5B%5D" => filter.run_ids.push(value.to_string()),
                "ancestor_run_id" => filter.ancestor_run_id = Some(value.to_string()),
                "include_self" => filter.include_self = value.eq_ignore_ascii_case("true"),
                "since" => filter.since = value.parse().unwrap_or_default(),
                _ => {}
            }
        }
        filter
    }

    fn matches(&self, state: &AgentApiState, event: &AgentRunEvent) -> bool {
        if event.sequence <= self.since {
            return false;
        }
        if !self.run_ids.is_empty() {
            return self.run_ids.iter().any(|run_id| run_id == &event.run_id);
        }
        self.ancestor_run_id.as_deref().is_some_and(|ancestor| {
            state.matches_ancestor(ancestor, self.include_self, &event.run_id)
        })
    }
}

fn event_to_sse(event: AgentRunEvent) -> Result<Event, Infallible> {
    Ok(Event::default()
        .json_data(event)
        .expect("agent event should serialize"))
}

pub async fn stream_events(
    State(state): State<std::sync::Arc<AppState>>,
    RawQuery(raw_query): RawQuery,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let filter = EventFilter::from_query(raw_query.as_deref());
    let replay = state
        .agent_api
        .events
        .read()
        .expect("agent events lock poisoned")
        .iter()
        .filter(|event| filter.matches(&state.agent_api, event))
        .cloned()
        .map(event_to_sse)
        .collect::<Vec<_>>();

    let live_state = state.agent_api.clone();
    let live_filter = filter.clone();
    let live =
        BroadcastStream::new(state.agent_api.event_sender.subscribe()).filter_map(move |result| {
            match result {
                Ok(event) if live_filter.matches(&live_state, &event) => Some(event_to_sse(event)),
                _ => None,
            }
        });

    Sse::new(stream::iter(replay).chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("local-proxy"),
    )
}

pub async fn list_runs() -> Json<Value> {
    Json(json!({
        "runs": [],
        "page_info": {
            "has_next_page": false,
            "next_cursor": null
        }
    }))
}

pub async fn get_run(
    State(state): State<std::sync::Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let events = state
        .agent_api
        .events
        .read()
        .expect("agent events lock poisoned");
    let run_events = events
        .iter()
        .filter(|event| event.run_id == run_id)
        .collect::<Vec<_>>();
    let state_name = run_events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "run_started" => Some("INPROGRESS"),
            "run_succeeded" => Some("SUCCEEDED"),
            "run_failed" => Some("FAILED"),
            "run_cancelled" => Some("CANCELLED"),
            _ => None,
        })
        .unwrap_or("SUCCEEDED");
    let last_event_sequence = run_events.iter().map(|event| event.sequence).max();
    drop(events);

    Json(json!({
        "task_id": run_id,
        "parent_run_id": null,
        "title": "Local agent",
        "state": state_name,
        "prompt": "",
        "created_at": now_rfc3339(),
        "started_at": null,
        "updated_at": now_rfc3339(),
        "run_time": null,
        "status_message": null,
        "source": "ORCHESTRATION",
        "execution_location": "LOCAL",
        "session_id": null,
        "session_link": null,
        "creator": null,
        "executor": null,
        "conversation_id": null,
        "request_usage": null,
        "is_sandbox_running": false,
        "agent_config_snapshot": null,
        "artifacts": [],
        "last_event_sequence": last_event_sequence,
        "children": state.agent_api.children(&run_id)
    }))
}

#[derive(Debug, Deserialize)]
pub struct ReportEventRequest {
    event_type: String,
    #[serde(default)]
    execution_id: Option<String>,
    #[serde(default)]
    ref_id: Option<String>,
}

pub async fn report_event(
    State(state): State<std::sync::Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(request): Json<ReportEventRequest>,
) -> Json<Value> {
    let event = state.agent_api.emit(
        run_id,
        request.event_type,
        request.ref_id,
        request.execution_id,
    );
    Json(json!({ "sequence": event.sequence }))
}

pub async fn acknowledge() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Debug, Deserialize)]
pub struct SendMessagesRequest {
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    sender_run_id: String,
}

pub async fn send_messages(
    State(state): State<std::sync::Arc<AppState>>,
    Json(request): Json<SendMessagesRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let recipients = request
        .to
        .iter()
        .map(|address| {
            state
                .resolve_agent_address(address)
                .map(|run_id| (address.clone(), run_id))
                .ok_or_else(|| address.clone())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|address| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "code": "LOCAL_PROXY_UNKNOWN_AGENT",
                        "message": format!(
                            "Unknown agent address '{address}'. Sending a message does not create an agent; multi-level orchestration is unavailable."
                        )
                    }
                })),
            )
        })?;
    if recipients.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "LOCAL_PROXY_UNKNOWN_AGENT",
                    "message": "At least one registered agent address is required."
                }
            })),
        ));
    }

    let mut message_ids = Vec::with_capacity(recipients.len());
    for (address, recipient_run_id) in recipients {
        state.mark_agent_message_sent(&address);
        let message_id = state.agent_api.store_message(
            recipient_run_id,
            request.sender_run_id.clone(),
            request.subject.clone(),
            request.body.clone(),
        );
        message_ids.push(message_id);
    }
    Ok(Json(json!({ "message_ids": message_ids })))
}

pub async fn list_messages(
    State(state): State<std::sync::Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let messages = state
        .agent_api
        .messages
        .read()
        .expect("agent messages lock poisoned")
        .values()
        .filter(|message| {
            message
                .recipients
                .iter()
                .any(|recipient| recipient == &run_id)
        })
        .map(|message| {
            json!({
                "message_id": message.message_id,
                "sender_run_id": message.sender_run_id,
                "subject": message.subject,
                "sent_at": message.sent_at,
                "delivered_at": message.delivered_at,
                "read_at": message.read_at
            })
        })
        .collect::<Vec<_>>();
    Json(json!(messages))
}

pub async fn mark_message_delivered(
    State(state): State<std::sync::Arc<AppState>>,
    Path(message_id): Path<String>,
) -> StatusCode {
    if let Some(message) = state
        .agent_api
        .messages
        .write()
        .expect("agent messages lock poisoned")
        .get_mut(&message_id)
    {
        message.delivered_at = Some(now_rfc3339());
    }
    StatusCode::NO_CONTENT
}

pub async fn read_message(
    State(state): State<std::sync::Arc<AppState>>,
    Path(message_id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let mut messages = state
        .agent_api
        .messages
        .write()
        .expect("agent messages lock poisoned");
    let message = messages.get_mut(&message_id).ok_or(StatusCode::NOT_FOUND)?;
    message.read_at = Some(now_rfc3339());
    Ok(Json(json!({
        "message_id": message.message_id,
        "sender_run_id": message.sender_run_id,
        "subject": message.subject,
        "body": message.body,
        "sent_at": message.sent_at,
        "delivered_at": message.delivered_at,
        "read_at": message.read_at
    })))
}

pub async fn list_identities() -> Json<Value> {
    Json(json!({ "agents": [] }))
}

pub async fn list_connected_workers() -> Json<Value> {
    Json(json!({ "workers": [] }))
}

pub async fn cancel_task(Path(_task_id): Path<String>) -> Json<Value> {
    Json(json!("cancelled"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthStyle, Config};

    fn app_state() -> std::sync::Arc<AppState> {
        std::sync::Arc::new(AppState::new(
            Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                backend_base_url: "http://127.0.0.1:3113/v1".into(),
                backend_auth_style: AuthStyle::Bearer,
                backend_api_key: None,
                azure_api_version: None,
                default_model: "test-model".into(),
            },
            vec![],
        ))
    }

    #[test]
    fn event_filter_supports_repeated_run_ids() {
        let filter = EventFilter::from_query(Some("run_ids[]=run-a&run_ids[]=run-b&since=4"));
        let state = AgentApiState::new();
        let event = AgentRunEvent {
            event_type: "run_started".into(),
            run_id: "run-b".into(),
            ref_id: None,
            execution_id: None,
            occurred_at: now_rfc3339(),
            sequence: 5,
        };

        assert!(filter.matches(&state, &event));
    }

    #[test]
    fn ancestor_filter_tracks_reported_children() {
        let state = AgentApiState::new();
        state.emit(
            "parent".into(),
            "child_agent_started".into(),
            Some("child".into()),
            None,
        );
        let filter =
            EventFilter::from_query(Some("ancestor_run_id=parent&include_self=true&since=0"));
        let child_event = state.emit("child".into(), "run_succeeded".into(), None, None);

        assert!(filter.matches(&state, &child_event));
    }

    #[tokio::test]
    async fn sending_to_an_unknown_agent_is_rejected() {
        let result = send_messages(
            State(app_state()),
            Json(SendMessagesRequest {
                to: vec!["missing-child".into()],
                subject: "work".into(),
                body: "do work".into(),
                sender_run_id: "parent".into(),
            }),
        )
        .await;

        let (status, body) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0["error"]["code"],
            serde_json::Value::String("LOCAL_PROXY_UNKNOWN_AGENT".into())
        );
    }

    #[tokio::test]
    async fn sending_to_a_conversation_alias_notifies_the_stable_run() {
        let state = app_state();
        state.register_conversation_task("child-conversation", "child-run");

        let _ = send_messages(
            State(state.clone()),
            Json(SendMessagesRequest {
                to: vec!["child-conversation".into()],
                subject: "work".into(),
                body: "do work".into(),
                sender_run_id: "parent".into(),
            }),
        )
        .await
        .unwrap();

        let events = state
            .agent_api
            .events
            .read()
            .expect("agent events lock poisoned");
        assert_eq!(events.last().unwrap().event_type, "new_message");
        assert_eq!(events.last().unwrap().run_id, "child-run");
        drop(events);
        assert!(
            state
                .conversation_cache_target("child-conversation")
                .unwrap()
                .1
                > 0
        );
    }
}
