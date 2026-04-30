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

/// Confirmed observed in real `warp-oss login` integration test: the client
/// fires this several times during login to push privacy / settings deltas to
/// the server. We accept and discard.
pub fn update_user_settings() -> Value {
    json!({
        "updateUserSettings": {
            "__typename": "UpdateUserSettingsOutput",
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}

/// Confirmed observed during launch: the cloud-objects subscription fetches
/// updates since a cursor. Real cynic shape (see
/// crates/graphql/src/api/queries/get_updated_cloud_objects.rs::UpdatedCloudObjectsOutput):
///   { actionHistories?, deletedObjectUids, folders?, genericStringObjects?,
///     mcpGallery?, notebooks?, responseContext, userProfiles?, workflows? }
/// All `Option<Vec<...>>` fields can be null; only deletedObjectUids and
/// responseContext are required to be present (and themselves have all-Option
/// inner fields).
pub fn get_updated_cloud_objects() -> Value {
    json!({
        "updatedCloudObjects": {
            "__typename": "UpdatedCloudObjectsOutput",
            "actionHistories": [],
            "deletedObjectUids": {
                "folderUids": [],
                "genericStringObjectUids": [],
                "notebookUids": [],
                "workflowUids": []
            },
            "folders": [],
            "genericStringObjects": [],
            "mcpGallery": [],
            "notebooks": [],
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" },
            "userProfiles": [],
            "workflows": []
        }
    })
}

pub fn empty_ok(operation_name: &str) -> Value {
    json!({ operation_name: { "__typename": "Output" } })
}
