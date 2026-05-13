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
    let choices: Vec<Value> = ids.iter().map(|id| llm_info(id, id)).collect();
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
        "llmSettings": {
            "enabled": true,
            // Cynic LlmSettings has a second field — Vec<LlmHostSettingsEntry> —
            // listing which host backends are allowed and their per-host
            // settings. Empty list = use defaults for everything.
            "hostConfigs": []
        },
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
        "usageBasedPricingSettings": { "enabled": false, "maxMonthlySpendCents": null },
        "addonCreditsSettings": {
            "autoReloadEnabled": false,
            "maxMonthlySpendCents": null,
            "selectedAutoReloadCreditDenomination": null
        },
        "codebaseContextSettings": {
            "enabled": false,
            "setting": "DISABLE"
        },
        "sandboxedAgentSettings": null,
        "ambientAgentSettings": null
    })
}

/// Far-future RFC 3339 timestamp the proxy uses for "never expires" /
/// "next refresh" type fields. cynic's `Time` deserializer rejects null for
/// non-Optional fields, so we hand it a real string.
const FAR_FUTURE_TIME: &str = "2099-12-31T23:59:59Z";

/// Public alias of [`FAR_FUTURE_TIME`] for use by the cloud_stubs module.
pub const FAR_FUTURE_TIME_PUBLIC: &str = FAR_FUTURE_TIME;

fn billing_tier() -> Value {
    // Real cynic Tier struct (crates/graphql/src/api/billing.rs:Tier) has
    // 18 fields. The pricing / anyoneWithLinkSharing / directLinkSharing
    // fields visible in the legacy .graphql comment are NOT in the current
    // cynic struct, so we omit them. All Option<...> policies are null.
    json!({
        "name": "Local",
        "description": "warp_local_proxy local-mode tier",
        "warpAiPolicy": {
            "limit": -1,
            "isCodeSuggestionsToggleable": true,
            "isPromptSuggestionsToggleable": true,
            "isNextCommandEnabled": true,
            "isGitOperationsAiEnabled": true,
            "isVoiceEnabled": false
        },
        "teamSizePolicy": { "isUnlimited": true, "limit": 0 },
        "sharedNotebooksPolicy": { "isUnlimited": true, "limit": 0 },
        "sharedWorkflowsPolicy": { "isUnlimited": true, "limit": 0 },
        "sessionSharingPolicy": { "enabled": false, "maxSessionBytesSize": 0 },
        "aiAutonomyPolicy": { "enabled": true, "toggleable": true },
        "telemetryDataCollectionPolicy": null,
        "ugcDataCollectionPolicy": null,
        "usageBasedPricingPolicy": null,
        "codebaseContextPolicy": null,
        "byoApiKeyPolicy": { "enabled": true },
        "purchaseAddOnCreditsPolicy": null,
        "enterprisePayAsYouGoPolicy": null,
        "enterpriseCreditsAutoReloadPolicy": null,
        "multiAdminPolicy": null,
        "ambientAgentsPolicy": null
    })
}

fn billing_metadata() -> Value {
    json!({
        // [FORK] Must be a paid CustomerType (BUILD / TURBO / BUSINESS / etc.)
        // so is_user_on_paid_plan() returns true. Otherwise the AI prompt-alert
        // shows "to use AI, enable analytics or upgrade" because telemetry is
        // disabled in this fork. "INDIVIDUAL" was an invalid value that the
        // cynic deserializer mapped to CustomerType::Unknown (the #[cynic(fallback)]
        // variant), which the upstream is_user_on_paid_plan check treats as
        // not-on-paid. See app/src/workspaces/workspace.rs:502 and
        // app/src/ai/blocklist/prompt/prompt_alert.rs:142.
        "customerType": "BUILD",
        "delinquencyStatus": "NONE",
        "tier": billing_tier(),
        "serviceAgreements": [],
        "aiOverages": null
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
        // Real shape: BonusGrantsInfo { grants: [BonusGrant], spendingInfo: BonusGrantSpendingInfo? }
        "bonusGrantsInfo": { "grants": [], "spendingInfo": null },
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
                // [FORK] Set anonymousUserInfo to null so is_user_anonymous()
                // returns false. With it set to NATIVE_CLIENT_ANONYMOUS_USER +
                // linkedAt=null (the natural shape after CreateAnonymousUser),
                // the GUI permanently shows "Sign up to use AI" and refuses
                // to enable AI features. In local-proxy mode there's no real
                // anonymous-vs-full-user distinction; we always claim "fully
                // signed-in local user" regardless of how the auth flow got
                // here.
                "anonymousUserInfo": null,
                "experiments": [],
                "globalSkills": [],
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
                "overages": { "pricePerRequestUsdCents": 0 },
                // Required by cynic PricingInfo fragment.
                "addonCreditsOptions": []
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
    // Real cynic shape from crates/graphql/src/api/ai.rs::RequestLimitInfo
    // has 12 required fields (none Optional). cynic's Time deserializer
    // rejects null, so nextRefreshTime needs a real RFC 3339 string.
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "workspaces": [{
                    "uid": LOCAL_WORKSPACE_UID,
                    "bonusGrantsInfo": { "grants": [], "spendingInfo": null }
                }],
                "requestLimitInfo": {
                    "isUnlimited": true,
                    "nextRefreshTime": FAR_FUTURE_TIME,
                    "requestLimit": 999999,
                    "requestsUsedSinceLastRefresh": 0,
                    "requestLimitRefreshDuration": "MONTHLY",
                    "isUnlimitedVoice": true,
                    "voiceRequestLimit": 0,
                    "voiceRequestsUsedSinceLastRefresh": 0,
                    "isUnlimitedCodebaseIndices": true,
                    "maxCodebaseIndices": 0,
                    "maxFilesPerRepo": 999999,
                    "embeddingGenerationBatchSize": 100
                },
                "bonusGrants": []
            }
        }
    })
}

pub fn get_referral_info() -> Value {
    // user { referrals { referralCode numberClaimed isReferred } }
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "referrals": {
                    "referralCode": "",
                    "numberClaimed": 0,
                    "isReferred": false
                }
            }
        }
    })
}

pub fn user_github_info() -> Value {
    // Inline-fragment union: GithubConnectedOutput | GithubAuthRequiredOutput.
    // We claim "auth required" with empty install link so the UI shows the
    // connect-GitHub action without trying to render an integrated repo list.
    json!({
        "userGithubInfo": {
            "__typename": "GithubAuthRequiredOutput",
            "authUrl": "",
            "txId": "local-mode",
            "appInstallLink": ""
        }
    })
}

pub fn get_conversation_usage() -> Value {
    // user { conversationUsage: [...] } — empty list is fine for local mode.
    json!({
        "user": {
            "__typename": "UserOutput",
            "user": {
                "conversationUsage": []
            }
        }
    })
}

pub fn get_experiments() -> Value {
    json!({ "experiments": [] })
}

/// `MintCustomToken` mutation — the "Sign in to use AI" button in Settings
/// dispatches `MainPageAction::SignupAnonymousUser`, which calls this mutation
/// to mint a fresh Firebase custom token. We hand back the same fake JWT shape
/// the OAuth device-flow stub returns; the warp client will turn it into a
/// FirebaseToken::Custom and exchange it via /proxy/customToken.
pub fn mint_custom_token() -> Value {
    json!({
        "mintCustomToken": {
            "__typename": "MintCustomTokenOutput",
            // Same alg=none JWT shape we use elsewhere — keeps the signature
            // verification path consistent (the warp client never verifies).
            "customToken": "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJpc3MiOiJ3YXJwX2xvY2FsX3Byb3h5Iiwic3ViIjoibG9jYWwtdXNlci11aWQiLCJhdWQiOiJ3YXJwLWNsaSIsImVtYWlsIjoibG9jYWxAbG9jYWwifQ.",
            "responseContext": response_context()
        }
    })
}
