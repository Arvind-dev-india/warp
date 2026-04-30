//! Canned local-mode responses for the launch-path GraphQL operations
//! (identity, settings, workspaces, model lists, experiments).
//!
//! Shapes are derived from the cynic types in `crates/graphql/src/api/`. JSON
//! field names are camelCase to match the GraphQL wire format that cynic
//! deserializers expect.
//!
//! Every inline-fragment-discriminated object includes `__typename` because
//! cynic's `InlineFragments` enums dispatch on it.

use serde_json::{json, Value};

use crate::server::AppState;

const LOCAL_USER_UID: &str = "local-user-uid";
const LOCAL_WORKSPACE_UID: &str = "local-workspace-uid";
const LOCAL_TEAM_UID: &str = "local-team-uid";

/// Wraps a single model id into the cynic-expected `LlmInfo` shape.
fn llm_info(id: &str, display_name: &str) -> Value {
    json!({
        "displayName": display_name,
        "baseModelName": display_name,
        "id": id,
        "reasoningLevel": null,
        "usageMetadata": {
            "creditMultiplier": 1.0,
            "requestMultiplier": 1
        },
        "description": "Routed through warp_local_proxy to your configured backend.",
        "disableReason": null,
        "visionSupported": false,
        "spec": { "cost": 0.0, "quality": 0.0, "speed": 0.0 },
        "provider": "OPENAI",
        "hostConfigs": [
            { "enabled": true, "modelRoutingHost": "DIRECT_API" }
        ],
        "pricing": { "discountPercentage": null },
        "contextWindow": {
            "isConfigurable": false,
            "min": 1024,
            "max": 200000,
            "default": 128000
        }
    })
}

fn available_llms(default_id: &str, choices: Vec<Value>) -> Value {
    json!({
        "defaultId": default_id,
        "choices": choices,
        "preferredCodexModelId": null
    })
}

/// Builds the `FeatureModelChoice` shape used by GetUser, GetFeatureModelChoices,
/// FreeAvailableModels, and the Workspace embedded in
/// GetWorkspacesMetadataForUser.
///
/// Populates each "feature" (agentMode / planning / coding / cliAgent /
/// computerUseAgent) with one LlmInfo per id the proxy fetched from the
/// backend's `/v1/models` endpoint at startup. Falls back to a single
/// `LOCAL_FALLBACK_MODEL_ID` entry when the backend list was empty.
fn feature_model_choice(state: &AppState) -> Value {
    let ids = state.advertised_models();
    let default_id = state.default_model_id();
    let choices: Vec<Value> = ids
        .iter()
        .map(|id| llm_info(id, id))
        .collect();
    let entries = || -> Value { available_llms(&default_id, choices.clone()) };
    json!({
        "agentMode": entries(),
        "planning": entries(),
        "coding": entries(),
        "cliAgent": entries(),
        "computerUseAgent": entries()
    })
}

fn workspace_member(uid: &str, email: &str) -> Value {
    json!({
        "uid": uid,
        "email": email,
        "role": "OWNER",
        "usageInfo": {
            "isUnlimited": true,
            "requestLimit": 0,
            "requestsUsedSinceLastRefresh": 0,
            "isRequestLimitProrated": false
        }
    })
}

fn workspace_settings() -> Value {
    json!({
        "isDiscoverable": false,
        "isInviteLinkEnabled": false,
        "llmSettings": { "enabled": true },
        "telemetrySettings": { "forceEnabled": false },
        "ugcCollectionSettings": { "setting": "DISABLE" },
        "cloudConversationStorageSettings": { "setting": "DISABLE" },
        "aiPermissionsSettings": {
            "allowAiInRemoteSessions": true,
            "remoteSessionRegexList": []
        },
        "linkSharingSettings": {
            "anyoneWithLinkSharingEnabled": false,
            "directLinkSharingEnabled": false
        },
        "secretRedactionSettings": { "enabled": true, "regexes": [] },
        "aiAutonomySettings": {
            "applyCodeDiffsSetting": "RESPECT_USER_SETTING",
            "readFilesSetting": "RESPECT_USER_SETTING",
            "readFilesAllowlist": null,
            "createPlansSetting": "RESPECT_USER_SETTING",
            "executeCommandsSetting": "RESPECT_USER_SETTING",
            "executeCommandsAllowlist": null,
            "executeCommandsDenylist": null,
            "writeToPtySetting": "RESPECT_USER_SETTING",
            "computerUseSetting": "RESPECT_USER_SETTING"
        },
        "usageBasedPricingSettings": { "enabled": false },
        "addonCreditsSettings": { "enabled": false },
        "codebaseContextSettings": {
            "enabled": false,
            "setting": "DISABLE"
        },
        "sandboxedAgentSettings": null,
        "ambientAgentSettings": null
    })
}

fn billing_metadata() -> Value {
    json!({
        "customerType": "INDIVIDUAL",
        "delinquencyStatus": "NONE",
        "tier": {
            "name": "Local",
            "description": "warp_local_proxy local-mode tier",
            "warpAiPolicy": {
                "limit": -1,
                "isCodeSuggestionsToggleable": true,
                "isPromptSuggestionsToggleable": true,
                "isNextCommandEnabled": true,
                "isVoiceEnabled": false
            },
            "teamSizePolicy": { "isUnlimited": true, "limit": 0 },
            "sharedNotebooksPolicy": { "isUnlimited": true, "limit": 0 },
            "sharedWorkflowsPolicy": { "isUnlimited": true, "limit": 0 },
            "sessionSharingPolicy": { "enabled": false, "maxSessionBytesSize": 0 },
            "anyoneWithLinkSharingPolicy": { "toggleable": false },
            "directLinkSharingPolicy": { "toggleable": false },
            "byoApiKeyPolicy": { "enabled": true },
            "pricing": {
                "enablePayAsYouGo": false,
                "autoReloadCreditDenomination": 0,
                "autoReloadCostCents": 0
            }
        },
        "serviceAgreements": []
    })
}

fn workspace_obj(state: &AppState) -> Value {
    json!({
        "uid": LOCAL_WORKSPACE_UID,
        "name": "Local",
        "stripeCustomerId": null,
        "members": [workspace_member(LOCAL_USER_UID, "local@local")],
        "teams": [
            {
                "uid": LOCAL_TEAM_UID,
                "name": "Local",
                "members": [{
                    "uid": LOCAL_USER_UID,
                    "email": "local@local",
                    "role": "OWNER"
                }]
            }
        ],
        "billingMetadata": billing_metadata(),
        "bonusGrantsInfo": { "totalGrants": 0, "remainingGrants": 0 },
        "settings": workspace_settings(),
        "hasBillingHistory": false,
        "inviteCode": null,
        "pendingEmailInvites": [],
        "inviteLinkDomainRestrictions": [],
        "isEligibleForDiscovery": false,
        "featureModelChoice": feature_model_choice(state),
        "totalRequestsUsedSinceLastRefresh": 0
    })
}

fn user_profile() -> Value {
    json!({
        "displayName": "Local User",
        "email": "local@local",
        "needsSsoLink": false,
        "photoUrl": null,
        "uid": LOCAL_USER_UID
    })
}

fn anonymous_user_info() -> Value {
    json!({
        "anonymousUserType": "NATIVE_CLIENT_ANONYMOUS_USER",
        "linkedAt": null,
        "personalObjectLimits": {
            "envVarLimit": 999999,
            "notebookLimit": 999999,
            "workflowLimit": 999999
        }
    })
}

fn response_context() -> Value {
    json!({ "serverVersion": "warp_local_proxy/0.1.0" })
}

// === Public per-operation handlers ===========================================

pub fn create_anonymous_user() -> Value {
    json!({
        "createAnonymousUser": {
            "__typename": "CreateAnonymousUserOutput",
            "expiresAt": null,
            "anonymousUserType": "NATIVE_CLIENT_ANONYMOUS_USER",
            "firebaseUid": LOCAL_USER_UID,
            "idToken": "local-mode-token",
            "isInviteValid": true,
            "responseContext": response_context()
        }
    })
}

pub fn get_user(state: &AppState) -> Value {
    json!({
        "user": {
            "__typename": "UserOutput",
            "apiKeyOwnerType": null,
            "principalType": "USER",
            "user": {
                "anonymousUserInfo": anonymous_user_info(),
                "experiments": [],
                "isOnboarded": true,
                "isOnWorkDomain": false,
                "profile": user_profile(),
                "llms": feature_model_choice(state)
            }
        }
    })
}

pub fn get_user_settings() -> Value {
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "settings": {
                    "isCloudConversationStorageEnabled": false,
                    "isCrashReportingEnabled": false,
                    "isTelemetryEnabled": false
                }
            }
        }
    })
}

pub fn get_workspaces_metadata_for_user(state: &AppState) -> Value {
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "workspaces": [workspace_obj(state)],
                "experiments": [],
                "discoverableTeams": []
            }
        },
        "pricingInfo": {
            "__typename": "PricingInfoOutput",
            "pricingInfo": {
                "plans": [],
                "overages": { "pricePerRequestUsdCents": 0 }
            }
        }
    })
}

pub fn get_feature_model_choices(state: &AppState) -> Value {
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "workspaces": [{
                    "featureModelChoice": feature_model_choice(state)
                }]
            }
        }
    })
}

pub fn free_available_models(state: &AppState) -> Value {
    json!({
        "freeAvailableModels": {
            "__typename": "FreeAvailableModelsOutput",
            "featureModelChoice": feature_model_choice(state),
            "responseContext": response_context()
        }
    })
}

pub fn get_request_limit_info() -> Value {
    json!({
        "requestLimitInfo": {
            "__typename": "RequestLimitInfoOutput",
            "info": {
                "isUnlimited": true,
                "requestLimit": 0,
                "requestsUsedSinceLastRefresh": 0,
                "nextRefreshTime": null
            },
            "responseContext": response_context()
        }
    })
}

pub fn get_experiments() -> Value {
    json!({ "experiments": [] })
}
