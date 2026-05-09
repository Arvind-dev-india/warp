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
//!
//! Some handlers ([`bulk_create_objects`], [`create_generic_string_object`])
//! receive the raw GraphQL variables and echo the client's input back so the
//! client thinks the object was successfully created in the cloud.

use serde_json::{json, Value};

use crate::operations::canned::FAR_FUTURE_TIME_PUBLIC;

pub fn list_ambient_agent_tasks() -> Value {
    json!({ "listAmbientAgentTasks": { "__typename": "AmbientAgentTaskListOutput", "tasks": [] } })
}

pub fn list_ai_conversation_metadata() -> Value {
    json!({
        "listAIConversations": {
            "__typename": "ListAIConversationsOutput",
            "conversations": [],
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}

pub fn update_event_sequence_on_server() -> Value {
    json!({ "updateEventSequence": { "__typename": "UpdateEventSequenceOutput" } })
}

pub fn update_user_settings() -> Value {
    json!({
        "updateUserSettings": {
            "__typename": "UpdateUserSettingsOutput",
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}

/// `GetUpdatedCloudObjects` — cynic shape from
/// crates/graphql/src/api/queries/get_updated_cloud_objects.rs.
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

fn local_space() -> Value {
    json!({
        "__typename": "Space",
        "uid": "local-space",
        "type": "User"
    })
}

fn object_metadata(uid: &str) -> Value {
    json!({
        "creatorUid": "local-user-uid",
        "currentEditorUid": "local-user-uid",
        "isWelcomeObject": false,
        "lastEditorUid": "local-user-uid",
        "metadataLastUpdatedTs": FAR_FUTURE_TIME_PUBLIC,
        // Container is an inline-fragment union (FolderContainer | Space).
        "parent": local_space(),
        "revisionTs": FAR_FUTURE_TIME_PUBLIC,
        "trashedTs": null,
        "uid": uid
    })
}

fn object_permissions() -> Value {
    json!({
        "guests": [],
        "lastUpdatedTs": FAR_FUTURE_TIME_PUBLIC,
        "anyoneLinkSharing": null,
        "space": local_space()
    })
}

/// Echoes one input GenericStringObjectInput back as a
/// CreateGenericStringObjectOutput so the client thinks the object was
/// created in the cloud and stops retrying. The `clientId` MUST be the same
/// `client_id` the input sent (the client uses it to match request→response);
/// otherwise the client logs "invalid client id" and retries forever.
fn echo_create_output(input: &Value, fallback_uid: &str) -> Value {
    let format = input
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("JsonPreference");
    let serialized = input
        .get("serializedModel")
        .or_else(|| input.get("serialized_model"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Echo the client_id (camelCase from cynic InputObject `client_id`).
    let client_id = input
        .get("clientId")
        .or_else(|| input.get("client_id"))
        .and_then(|v| v.as_str())
        .unwrap_or(fallback_uid)
        .to_string();
    json!({
        "__typename": "CreateGenericStringObjectOutput",
        "clientId": client_id,
        "genericStringObject": {
            "format": format,
            "metadata": object_metadata(&client_id),
            "permissions": object_permissions(),
            "serializedModel": serialized
        },
        "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" },
        "revisionTs": FAR_FUTURE_TIME_PUBLIC
    })
}

/// `BulkCreateObjects` mutation — observed during launch as part of cloud
/// preferences sync. The client retries until the response contains an
/// `objects` array mirroring the input (so it can mark each preference as
/// "synced to cloud"). We echo the input back.
pub fn bulk_create_objects(variables: &Value) -> Value {
    let inputs: Vec<Value> = variables
        .get("input")
        .and_then(|i| i.get("generic_string_objects"))
        .or_else(|| {
            variables
                .get("input")
                .and_then(|i| i.get("genericStringObjects"))
        })
        .and_then(|gso| gso.get("objects"))
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();

    let echoed: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(idx, obj)| echo_create_output(obj, &format!("local-bulk-{idx}")))
        .collect();

    json!({
        "bulkCreateObjects": {
            "__typename": "BulkCreateObjectsOutput",
            "genericStringObjects": {
                "__typename": "BulkCreateGenericStringObjectsOutput",
                "objects": echoed
            },
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}

/// `CreateGenericStringObject` mutation — single-object create. Echo the
/// input back as a successful CreateGenericStringObjectOutput.
pub fn create_generic_string_object(variables: &Value) -> Value {
    let input = variables
        .get("input")
        .and_then(|i| i.get("generic_string_object"))
        .or_else(|| {
            variables
                .get("input")
                .and_then(|i| i.get("genericStringObject"))
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    let echoed = echo_create_output(&input, "local-single-create");
    json!({
        "createGenericStringObject": echoed
    })
}

/// `GetCloudEnvironmentsQuery` — cloud-only feature, return empty list so
/// the UI doesn't error.
pub fn get_cloud_environments() -> Value {
    json!({
        "getCloudEnvironments": {
            "__typename": "GetCloudEnvironmentsOutput",
            "cloudEnvironments": [],
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}

/// `GetAvailableHarnesses` — returns an empty harness list.
/// The client uses this to populate the agent harness selector (Oz, ClaudeCode, Gemini).
/// In local mode we return an empty list so no cloud harnesses appear.
pub fn get_available_harnesses() -> Value {
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "availableHarnesses": {
                    "harnesses": []
                }
            }
        }
    })
}

/// `UpdateAgentTask` — echoes success so the client's TaskStatusSyncModel
/// doesn't log errors on every agent turn.
pub fn update_agent_task() -> Value {
    json!({
        "updateAgentTask": {
            "__typename": "UpdateAgentTaskOutput",
            "responseContext": { "serverVersion": "warp_local_proxy/0.1.0" }
        }
    })
}
