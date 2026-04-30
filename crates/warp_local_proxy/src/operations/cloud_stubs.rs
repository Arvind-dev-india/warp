//! Empty-`Ok` responses for cloud polling APIs that retry on error.
//!
//! Per the rubber-duck pass on Phase 2 (see plan.md), returning a `Err` for
//! these from the proxy creates polling churn:
//! - `list_ambient_agent_tasks` is polled every ~1s by ambient-agents UI.
//! - `list_ai_conversation_metadata` is fired on launch even when cloud
//!   conversation storage is off (upstream already returns empty in that case).
//! - `update_event_sequence_on_server` is fire-and-forget but logs every error.
//!
//! Reserve `Err(...)` for user-initiated actions where surfacing
//! "unsupported in local mode" is useful (spawn_agent, attachments, etc.).

use serde_json::{json, Value};

pub fn list_ambient_agent_tasks() -> Value {
    // The cynic side wraps this in a result enum; returning empty list
    // satisfies the most permissive shape.
    json!({ "listAmbientAgentTasks": { "__typename": "AmbientAgentTaskListOutput", "tasks": [] } })
}

pub fn list_ai_conversation_metadata() -> Value {
    json!({ "listAiConversations": { "__typename": "ListAiConversationsOutput", "conversations": [] } })
}

pub fn update_event_sequence_on_server() -> Value {
    json!({ "updateEventSequence": { "__typename": "UpdateEventSequenceOutput" } })
}

pub fn empty_ok(operation_name: &str) -> Value {
    json!({ operation_name: { "__typename": "Output" } })
}
