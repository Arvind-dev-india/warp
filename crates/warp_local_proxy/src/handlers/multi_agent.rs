//! Handler for `POST /ai/multi-agent` (and `/ai/passive-suggestions`).
//!
//! The Warp client sends a **protobuf** [`warp_multi_agent_api::Request`] body
//! and expects an **SSE** stream where each `data:` line is a
//! **base64-url-safe-encoded** protobuf [`warp_multi_agent_api::ResponseEvent`].
//!
//! ## Agentic tool-calling loop
//!
//! The loop is **server-driven, turn-by-turn**:
//! 1. Client sends Request (user query or tool results)
//! 2. Proxy calls LLM (with OpenAI function-calling)
//! 3. If LLM returns tool_calls → proxy emits ToolCall messages → Finished
//! 4. Client executes tools locally (shell, files, grep, etc.)
//! 5. Client sends NEW Request with ToolCallResults → goto 2
//! 6. If LLM returns text → proxy emits AgentOutput → Finished

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine as _;
use prost::Message;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{AuthStyle, Config};
use crate::server::AppState;
use crate::upstream::openai::apply_backend_auth;

// ── OpenAI tool definitions ──────────────────────────────────────────

fn current_timestamp() -> prost_types::Timestamp {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    }
}

fn task_description(query: Option<&str>) -> String {
    let query = query.unwrap_or("Local agent").trim();
    let query = if query.is_empty() {
        "Local agent"
    } else {
        query
    };
    query.chars().take(80).collect()
}

fn stream_identity(request: &warp_multi_agent_api::Request, task_id: &str) -> (String, String) {
    let conversation_id = request
        .metadata
        .as_ref()
        .map(|metadata| metadata.conversation_id.as_str())
        .filter(|conversation_id| !conversation_id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    (conversation_id, task_id.to_string())
}

fn system_prompt(parent_agent_id: Option<&str>) -> String {
    let mut prompt = "You are a helpful AI assistant integrated into the Warp terminal. You have \
                      access to tools that let you run shell commands, read files, edit files, \
                      search code, and more. Use tools when needed to help the user. \
                      When providing code changes, use the apply_file_diffs tool. \
                      When you need to understand existing code, use read_files or grep. \
                      Be concise but thorough."
        .to_string();
    if let Some(parent_agent_id) = parent_agent_id {
        prompt.push_str(&format!(
            " You are already a child agent. Your parent agent address is '{parent_agent_id}'. \
             Multi-level orchestration is unavailable, so you cannot create another agent. \
             send_message_to_agent only contacts already-registered agents and never creates one. \
             For ongoing coordination, send replies to the parent address above and then use \
             wait_for_events; replies arrive as agent messages. Your final text result is also \
             delivered automatically to the parent when you have not already messaged it. \
             If asked to spawn a child, state that nested delegation is unavailable once, complete \
             any remaining work yourself, and finish."
        ));
    }
    prompt
}

fn oz_harness() -> warp_multi_agent_api::Harness {
    warp_multi_agent_api::Harness {
        variant: Some(warp_multi_agent_api::harness::Variant::Oz(
            warp_multi_agent_api::harness::Oz {},
        )),
    }
}

fn openai_tools() -> serde_json::Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "run_shell_command",
                "description": "Run a shell command in the user's terminal.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute" },
                        "is_read_only": { "type": "boolean", "description": "Whether the command only reads data" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_to_long_running_shell_command",
                "description": "Write input to a running shell command.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "input": { "type": "string" },
                        "mode": { "type": "string", "enum": ["raw", "line", "block"] },
                        "command_id": { "type": "string" }
                    },
                    "required": ["input", "command_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_shell_command_output",
                "description": "Read output from a running shell command.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command_id": { "type": "string" },
                        "delay_seconds": { "type": "integer" },
                        "wait_for_completion": { "type": "boolean" }
                    },
                    "required": ["command_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "transfer_shell_command_control_to_user",
                "description": "Hand control of a running shell command to the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string" }
                    },
                    "required": ["reason"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_files",
                "description": "Read one or more files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "line_ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "start": { "type": "integer" },
                                                "end": { "type": "integer" }
                                            }
                                        }
                                    }
                                },
                                "required": ["name"]
                            }
                        }
                    },
                    "required": ["files"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_documents",
                "description": "Read one or more documents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "documents": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "document_id": { "type": "string" },
                                    "line_ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "start": { "type": "integer" },
                                                "end": { "type": "integer" }
                                            }
                                        }
                                    }
                                },
                                "required": ["document_id"]
                            }
                        }
                    },
                    "required": ["documents"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_file_diffs",
                "description": "Apply search/replace edits and create files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string" },
                        "diffs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "search": { "type": "string" },
                                    "replace": { "type": "string" }
                                },
                                "required": ["file_path", "search", "replace"]
                            }
                        },
                        "new_files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "content": { "type": "string" }
                                },
                                "required": ["file_path", "content"]
                            }
                        },
                        "deleted_files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string" }
                                },
                                "required": ["file_path"]
                            }
                        },
                        "v4a_updates": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "move_to": { "type": "string" },
                                    "hunks": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "change_context": { "type": "array", "items": { "type": "string" } },
                                                "pre_context": { "type": "string" },
                                                "old": { "type": "string" },
                                                "new": { "type": "string" },
                                                "post_context": { "type": "string" }
                                            }
                                        }
                                    }
                                },
                                "required": ["file_path"]
                            }
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_documents",
                "description": "Apply search/replace edits to documents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "diffs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "document_id": { "type": "string" },
                                    "search": { "type": "string" },
                                    "replace": { "type": "string" }
                                },
                                "required": ["document_id", "search", "replace"]
                            }
                        }
                    },
                    "required": ["diffs"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_documents",
                "description": "Create one or more documents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "new_documents": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string" },
                                    "title": { "type": "string" }
                                },
                                "required": ["content", "title"]
                            }
                        }
                    },
                    "required": ["new_documents"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for regex matches in files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "queries": { "type": "array", "items": { "type": "string" } },
                        "path": { "type": "string" }
                    },
                    "required": ["queries"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "file_glob",
                "description": "Find files matching glob patterns.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patterns": { "type": "array", "items": { "type": "string" } },
                        "search_dir": { "type": "string" },
                        "max_matches": { "type": "integer" },
                        "max_depth": { "type": "integer" },
                        "min_depth": { "type": "integer" }
                    },
                    "required": ["patterns"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_codebase",
                "description": "Semantically search the codebase.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "path_filters": { "type": "array", "items": { "type": "string" } },
                        "codebase_path": { "type": "string" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "insert_review_comments",
                "description": "Insert code review comments.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo_path": { "type": "string" },
                        "comments": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "comment_id": { "type": "string" },
                                    "author": { "type": "string" },
                                    "last_modified_timestamp": { "type": "string" },
                                    "comment_body": { "type": "string" },
                                    "parent_comment_id": { "type": "string" },
                                    "html_url": { "type": "string" },
                                    "location": {
                                        "type": "object",
                                        "properties": {
                                            "file_path": { "type": "string" },
                                            "line": {
                                                "type": "object",
                                                "properties": {
                                                    "diff_hunk": { "type": "string" },
                                                    "range": {
                                                        "type": "object",
                                                        "properties": {
                                                            "start": { "type": "integer" },
                                                            "end": { "type": "integer" }
                                                        }
                                                    },
                                                    "side": { "type": "string", "enum": ["new", "old"] }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        "base_branch": { "type": "string" }
                    },
                    "required": ["repo_path", "comments"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_code_review",
                "description": "Open the code review panel.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "suggest_plan",
                "description": "Suggest a plan to the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string" }
                    },
                    "required": ["summary"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "suggest_create_plan",
                "description": "Ask the user to create a plan.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ask_user_question",
                "description": "Ask the user one or more questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question_id": { "type": "string" },
                                    "question": { "type": "string" },
                                    "multiple_choice": {
                                        "type": "object",
                                        "properties": {
                                            "options": {
                                                "type": "array",
                                                "minItems": 1,
                                                "items": { "type": "string" }
                                            },
                                            "is_multiselect": { "type": "boolean" },
                                            "supports_other": { "type": "boolean" },
                                            "recommended_option_index": { "type": "integer" }
                                        },
                                        "required": ["options", "is_multiselect", "supports_other"]
                                    }
                                },
                                "required": ["question_id", "question", "multiple_choice"]
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "suggest_prompt",
                "description": "Suggest a follow-up prompt to the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The suggested prompt text" },
                        "label": { "type": "string", "description": "Short label for the prompt chip" },
                        "display_mode": {
                            "type": "string",
                            "enum": ["prompt_chip", "inline_banner"],
                            "description": "How to display: 'prompt_chip' (small chip) or 'inline_banner' (large banner with title/description)"
                        },
                        "title": { "type": "string", "description": "Title for inline_banner mode" },
                        "description": { "type": "string", "description": "Description for inline_banner mode" }
                    },
                    "required": ["text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_skill",
                "description": "Read a skill definition.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "skill_path": { "type": "string" },
                        "bundled_skill_id": { "type": "string" },
                        "name": { "type": "string" }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "call_mcp_tool",
                "description": "Call an MCP tool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "args": { "type": "object" },
                        "server_id": { "type": "string" }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_mcp_resource",
                "description": "Read an MCP resource.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "uri": { "type": "string" },
                        "server_id": { "type": "string" }
                    },
                    "required": ["uri"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "init_project",
                "description": "Initialize a project.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "use_computer",
                "description": "Perform computer use actions (mouse/keyboard/typing).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action_summary": { "type": "string", "description": "Human-readable summary of what the actions do" },
                        "actions": {
                            "type": "array",
                            "description": "List of actions to perform",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "type": { "type": "string", "enum": ["mouse_move", "mouse_down", "mouse_up", "mouse_wheel", "wait", "type_text", "key_down", "key_up"] },
                                    "x": { "type": "integer" },
                                    "y": { "type": "integer" },
                                    "button": { "type": "string", "enum": ["left", "right", "middle"] },
                                    "text": { "type": "string" },
                                    "key": { "type": "string" },
                                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                                    "duration_ms": { "type": "integer" }
                                },
                                "required": ["type"]
                            }
                        }
                    },
                    "required": ["action_summary", "actions"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "request_computer_use",
                "description": "Request permission from the user to perform computer use actions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_summary": { "type": "string", "description": "Description of what computer use is needed for" }
                    },
                    "required": ["task_summary"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "send_message_to_agent",
                "description": "Send a message to one or more agents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "addresses": { "type": "array", "items": { "type": "string" } },
                        "subject": { "type": "string" },
                        "message": { "type": "string" }
                    },
                    "required": ["addresses", "subject", "message"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "wait_for_events",
                "description": "Wait for replies or lifecycle events from already-running agents after sending them a message.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "idle_timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 300,
                            "description": "Maximum idle time to wait for new agent events"
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "upload_file_artifact",
                "description": "Upload a file as an artifact attached to the conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": { "type": "string", "description": "Path to the file to upload" },
                        "description": { "type": "string", "description": "Description of the artifact" }
                    },
                    "required": ["file_path", "description"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_agents",
                "description": "Run multiple agents in parallel to work on subtasks. Each agent gets its own conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "Overall summary of what the agents will accomplish" },
                        "base_prompt": { "type": "string", "description": "Base instructions shared by all agents" },
                        "agents": {
                            "type": "array",
                            "description": "List of agents to run",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string", "description": "Short name for this agent" },
                                    "prompt": { "type": "string", "description": "Specific task for this agent" },
                                    "title": { "type": "string", "description": "Display title for this agent's conversation" }
                                },
                                "required": ["name", "prompt"]
                            }
                        }
                    },
                    "required": ["summary", "base_prompt", "agents"]
                }
            }
        }
    ])
}

fn tool_type_for_openai_name(name: &str) -> Option<warp_multi_agent_api::ToolType> {
    use warp_multi_agent_api::ToolType;

    Some(match name {
        "run_shell_command" => ToolType::RunShellCommand,
        "write_to_long_running_shell_command" => ToolType::WriteToLongRunningShellCommand,
        "read_shell_command_output" => ToolType::ReadShellCommandOutput,
        "transfer_shell_command_control_to_user" => ToolType::TransferShellCommandControlToUser,
        "read_files" => ToolType::ReadFiles,
        "read_documents" => ToolType::ReadDocuments,
        "apply_file_diffs" => ToolType::ApplyFileDiffs,
        "edit_documents" => ToolType::EditDocuments,
        "create_documents" => ToolType::CreateDocuments,
        "grep" => ToolType::Grep,
        "file_glob" => ToolType::FileGlobV2,
        "search_codebase" => ToolType::SearchCodebase,
        "insert_review_comments" => ToolType::InsertReviewComments,
        "open_code_review" => ToolType::OpenCodeReview,
        "suggest_plan" => ToolType::SuggestPlan,
        "suggest_create_plan" => ToolType::SuggestCreatePlan,
        "ask_user_question" => ToolType::AskUserQuestion,
        "suggest_prompt" => ToolType::SuggestPrompt,
        "read_skill" => ToolType::ReadSkill,
        "call_mcp_tool" => ToolType::CallMcpTool,
        "read_mcp_resource" => ToolType::ReadMcpResource,
        "init_project" => ToolType::InitProject,
        "use_computer" => ToolType::UseComputer,
        "request_computer_use" => ToolType::RequestComputerUse,
        "send_message_to_agent" => ToolType::SendMessageToAgent,
        "wait_for_events" => ToolType::WaitForEvents,
        "fetch_conversation" => ToolType::FetchConversation,
        "upload_file_artifact" => ToolType::UploadFileArtifact,
        "run_agents" => ToolType::RunAgents,
        _ => return None,
    })
}

fn openai_tools_for_request(request: &warp_multi_agent_api::Request) -> serde_json::Value {
    let supported: HashSet<_> = request
        .settings
        .as_ref()
        .map(|settings| {
            settings
                .supported_tools
                .iter()
                .filter_map(|tool| warp_multi_agent_api::ToolType::try_from(*tool).ok())
                .collect()
        })
        .unwrap_or_default();
    let is_child_agent = request
        .metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.parent_agent_id.is_empty());
    let tools = openai_tools()
        .as_array()
        .expect("OpenAI tools must be an array")
        .iter()
        .filter(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                .and_then(tool_type_for_openai_name)
                .is_some_and(|tool_type| {
                    (supported.is_empty() || supported.contains(&tool_type))
                        && !(is_child_agent
                            && tool_type == warp_multi_agent_api::ToolType::RunAgents)
                })
        })
        .cloned()
        .collect();
    serde_json::Value::Array(tools)
}

// ── Protobuf SSE encoding ────────────────────────────────────────────

fn sse_line(event: &warp_multi_agent_api::ResponseEvent) -> String {
    let bytes = event.encode_to_vec();
    let b64 = URL_SAFE.encode(&bytes);
    format!("data: \"{b64}\"\n\n")
}

// ── Extract inputs from the Request ──────────────────────────────────

fn extract_user_query(request: &warp_multi_agent_api::Request) -> Option<String> {
    let input = request.input.as_ref()?;
    let input_type = input.r#type.as_ref()?;

    match input_type {
        warp_multi_agent_api::request::input::Type::UserInputs(user_inputs) => {
            for ui in &user_inputs.inputs {
                if let Some(ref input_oneof) = ui.input {
                    use warp_multi_agent_api::request::input::user_inputs::user_input::Input;
                    match input_oneof {
                        Input::UserQuery(q) => return Some(q.query.clone()),
                        Input::CliAgentUserQuery(q) => {
                            return q.user_query.as_ref().map(|uq| uq.query.clone());
                        }
                        _ => {}
                    }
                }
            }
            None
        }
        #[allow(deprecated)]
        warp_multi_agent_api::request::input::Type::UserQuery(q) => Some(q.query.clone()),
        _ => None,
    }
}

/// Check if the request is a SummarizeConversation request.
fn is_summarize_request(request: &warp_multi_agent_api::Request) -> bool {
    request
        .input
        .as_ref()
        .and_then(|i| i.r#type.as_ref())
        .map(|t| {
            matches!(
                t,
                warp_multi_agent_api::request::input::Type::SummarizeConversation(_)
            )
        })
        .unwrap_or(false)
}

fn extract_tool_results(
    request: &warp_multi_agent_api::Request,
    state: &AppState,
) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let Some(input) = request.input.as_ref() else {
        return results;
    };
    let Some(input_type) = input.r#type.as_ref() else {
        return results;
    };

    if let warp_multi_agent_api::request::input::Type::UserInputs(user_inputs) = input_type {
        for ui in &user_inputs.inputs {
            if let Some(ref input_oneof) = ui.input {
                use warp_multi_agent_api::request::input::user_inputs::user_input::Input;
                if let Input::ToolCallResult(tcr) = input_oneof {
                    let text = request_tool_call_result_to_text(tcr, state);
                    results.push((tcr.tool_call_id.clone(), text));
                }
            }
        }
    }

    results
}

fn extract_received_agent_messages(request: &warp_multi_agent_api::Request) -> Vec<String> {
    use warp_multi_agent_api::request::input::user_inputs::user_input::Input;

    let Some(warp_multi_agent_api::request::input::Type::UserInputs(user_inputs)) = request
        .input
        .as_ref()
        .and_then(|input| input.r#type.as_ref())
    else {
        return Vec::new();
    };

    user_inputs
        .inputs
        .iter()
        .filter_map(|input| {
            let Input::MessagesReceivedFromAgents(received) = input.input.as_ref()? else {
                return None;
            };
            Some(
                received
                    .messages
                    .iter()
                    .map(|message| {
                        format!(
                            "[Agent message {} from {}]\nSubject: {}\n{}",
                            message.message_id,
                            message.sender_agent_id,
                            message.subject,
                            message.message_body
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn register_launched_agents(request: &warp_multi_agent_api::Request, state: &AppState) {
    use warp_multi_agent_api::request::input::tool_call_result::Result;
    use warp_multi_agent_api::request::input::user_inputs::user_input::Input;
    use warp_multi_agent_api::run_agents_result::{agent_outcome, Outcome};

    let Some(warp_multi_agent_api::request::input::Type::UserInputs(user_inputs)) = request
        .input
        .as_ref()
        .and_then(|input| input.r#type.as_ref())
    else {
        return;
    };

    for user_input in &user_inputs.inputs {
        let Some(Input::ToolCallResult(tool_result)) = user_input.input.as_ref() else {
            continue;
        };
        let Some(Result::RunAgentsResult(result)) = tool_result.result.as_ref() else {
            continue;
        };
        let Some(Outcome::Launched(launched)) = result.outcome.as_ref() else {
            continue;
        };
        for agent in &launched.agents {
            if let Some(agent_outcome::Result::Launched(child)) = agent.result.as_ref() {
                state.register_launched_agent(&agent.name, &child.agent_id);
            }
        }
    }
}

/// Convert a request-level ToolCallResult to text for the LLM.
fn request_tool_call_result_to_text(
    tcr: &warp_multi_agent_api::request::input::ToolCallResult,
    state: &AppState,
) -> String {
    let Some(ref result) = tcr.result else {
        return "(no result)".to_string();
    };
    use warp_multi_agent_api::request::input::tool_call_result::Result as R;
    match result {
        R::RunShellCommand(r) => match &r.result {
            Some(warp_multi_agent_api::run_shell_command_result::Result::CommandFinished(f)) => {
                format!(
                    "Command: {}\nExit code: {}\nOutput:\n{}",
                    r.command, f.exit_code, f.output
                )
            }
            Some(
                warp_multi_agent_api::run_shell_command_result::Result::LongRunningCommandSnapshot(
                    s,
                ),
            ) => {
                format!(
                    "Command: {} (still running)\nOutput so far:\n{}",
                    r.command, s.output
                )
            }
            Some(warp_multi_agent_api::run_shell_command_result::Result::PermissionDenied(_)) => {
                format!("Command: {} — Permission denied by user", r.command)
            }
            None => {
                format!("Command: {} — (no structured result)", r.command)
            }
        },
        R::ReadFiles(r) => match &r.result {
            Some(warp_multi_agent_api::read_files_result::Result::TextFilesSuccess(s)) => s
                .files
                .iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>()
                .join("\n\n"),
            Some(warp_multi_agent_api::read_files_result::Result::AnyFilesSuccess(s)) => {
                use warp_multi_agent_api::any_file_content::Content;
                s.files
                    .iter()
                    .filter_map(|file| match file.content.as_ref() {
                        Some(Content::TextContent(text)) => {
                            Some(format!("=== {} ===\n{}", text.file_path, text.content))
                        }
                        Some(Content::BinaryContent(binary)) => Some(format!(
                            "=== {} ===\n(binary file, {} bytes)",
                            binary.file_path,
                            binary.data.len()
                        )),
                        None => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
            Some(warp_multi_agent_api::read_files_result::Result::Error(e)) => {
                format!("Error: {}", e.message)
            }
            _ => "(file read result)".to_string(),
        },
        R::Grep(r) => match &r.result {
            Some(warp_multi_agent_api::grep_result::Result::Success(s)) => s
                .matched_files
                .iter()
                .map(|f| {
                    let lines: Vec<String> = f
                        .matched_lines
                        .iter()
                        .map(|l| l.line_number.to_string())
                        .collect();
                    format!("{}:{}", f.file_path, lines.join(","))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Some(warp_multi_agent_api::grep_result::Result::Error(e)) => {
                format!("Grep error: {}", e.message)
            }
            None => "(no grep result)".to_string(),
        },
        R::FileGlobV2(r) => match &r.result {
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Success(s)) => s
                .matched_files
                .iter()
                .map(|f| f.file_path.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Error(e)) => {
                format!("Glob error: {}", e.message)
            }
            None => "(no glob result)".to_string(),
        },
        R::SearchCodebase(r) => match &r.result {
            Some(warp_multi_agent_api::search_codebase_result::Result::Success(s)) => s
                .files
                .iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>()
                .join("\n\n"),
            Some(warp_multi_agent_api::search_codebase_result::Result::Error(e)) => {
                format!("Search error: {}", e.message)
            }
            None => "(no search result)".to_string(),
        },
        R::ApplyFileDiffs(r) => match &r.result {
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Success(s)) => {
                let files: Vec<_> = s
                    .updated_files_v2
                    .iter()
                    .filter_map(|f| f.file.as_ref())
                    .map(|f| f.file_path.as_str())
                    .collect();
                format!("Applied diffs to: {}", files.join(", "))
            }
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Error(e)) => {
                format!("Diff error: {}", e.message)
            }
            None => "(no diff result)".to_string(),
        },
        R::SuggestPlan(_) => "Plan accepted.".to_string(),
        R::SuggestCreatePlan(r) => {
            if r.accepted {
                "Plan accepted.".into()
            } else {
                "Plan rejected.".into()
            }
        }
        R::RunAgentsResult(result) => {
            use warp_multi_agent_api::run_agents_result::Outcome;
            match result.outcome.as_ref() {
                Some(Outcome::Launched(launched)) => {
                    let agents = launched
                        .agents
                        .iter()
                        .map(|agent| {
                            use warp_multi_agent_api::run_agents_result::agent_outcome::Result;
                            match agent.result.as_ref() {
                                Some(Result::Launched(child)) => {
                                    let address = state
                                        .resolve_agent_address(&child.agent_id)
                                        .unwrap_or_else(|| child.agent_id.clone());
                                    format!("{}: launched with agent_address={address}", agent.name)
                                }
                                Some(Result::Failed(failure)) => {
                                    format!("{}: failed: {}", agent.name, failure.error)
                                }
                                None => format!("{}: launch status unavailable", agent.name),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!(
                        "Agents launched. Use the exact agent_address values below with \
                         send_message_to_agent. To test agent-to-agent messaging, pass the \
                         destination agent_address to the sending agent:\n{agents}"
                    )
                }
                Some(Outcome::Denied(denied)) => {
                    format!("Agent launch denied: {}", denied.reason)
                }
                Some(Outcome::Failure(failure)) => {
                    format!("Agent launch failed: {}", failure.error)
                }
                None => "Agent launch returned no outcome.".to_string(),
            }
        }
        R::SendMessageToAgent(result) => {
            use warp_multi_agent_api::send_message_to_agent_result::Result;
            match result.result.as_ref() {
                Some(Result::Success(success)) => {
                    format!("Message sent with id={}", success.message_id)
                }
                Some(Result::Error(error)) => format!("Message failed: {}", error.message),
                None => "Message returned no result.".to_string(),
            }
        }
        R::FetchConversation(result) => {
            use warp_multi_agent_api::fetch_conversation_result::Result;
            match result.result.as_ref() {
                Some(Result::Success(success)) => format!(
                    "Conversation materialized at {}. Read the files in that directory for the child result.",
                    success.directory_path
                ),
                Some(Result::Error(error)) => {
                    format!("Failed to fetch conversation: {}", error.message)
                }
                None => "Fetch conversation returned no result.".to_string(),
            }
        }
        R::WaitForEvents(_) => "Finished waiting for agent events.".to_string(),
        _ => "(tool result)".to_string(),
    }
}

fn json_to_line_ranges(
    value: &serde_json::Value,
) -> Vec<warp_multi_agent_api::FileContentLineRange> {
    value
        .as_array()
        .map(|ranges| {
            ranges
                .iter()
                .filter_map(|range| {
                    Some(warp_multi_agent_api::FileContentLineRange {
                        start: range.get("start")?.as_u64()? as u32,
                        end: range.get("end")?.as_u64()? as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_string_array(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn json_to_write_mode(
    mode: Option<&str>,
) -> Option<warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::Mode> {
    use warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::mode::Mode;
    use warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::Mode as WriteMode;

    mode.map(|mode| WriteMode {
        mode: Some(match mode {
            "line" => Mode::Line(()),
            "block" => Mode::Block(()),
            _ => Mode::Raw(()),
        }),
    })
}

fn json_to_prost_value(value: &serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(prost_types::NullValue::NullValue as i32),
        serde_json::Value::Bool(v) => Kind::BoolValue(*v),
        serde_json::Value::Number(v) => Kind::NumberValue(v.as_f64().unwrap_or_default()),
        serde_json::Value::String(v) => Kind::StringValue(v.clone()),
        serde_json::Value::Array(v) => Kind::ListValue(prost_types::ListValue {
            values: v.iter().map(json_to_prost_value).collect(),
        }),
        serde_json::Value::Object(v) => Kind::StructValue(prost_types::Struct {
            fields: v
                .iter()
                .map(|(key, value)| (key.clone(), json_to_prost_value(value)))
                .collect(),
        }),
    };

    prost_types::Value { kind: Some(kind) }
}

fn json_to_prost_struct(value: &serde_json::Value) -> Option<prost_types::Struct> {
    match value {
        serde_json::Value::Object(fields) => Some(prost_types::Struct {
            fields: fields
                .iter()
                .map(|(key, value)| (key.clone(), json_to_prost_value(value)))
                .collect(),
        }),
        _ => None,
    }
}

fn wait_for_conversation_command(path: &std::path::Path, required_version: u64) -> String {
    if cfg!(windows) {
        let path = path.to_string_lossy().replace('\'', "''");
        format!(
            "$path = '{path}'; $versionPath = \"$path.version\"; \
             $deadline = (Get-Date).AddSeconds(120); while ($true) {{ \
             $ready = $false; if ((Test-Path -LiteralPath $path) -and \
             (Test-Path -LiteralPath $versionPath)) {{ try {{ \
             $ready = ([uint64](Get-Content -Raw -LiteralPath $versionPath)) -ge {required_version} \
             }} catch {{ $ready = $false }} }}; if ($ready) {{ break }}; \
             if ((Get-Date) -ge $deadline) {{ throw 'Timed out waiting for child conversation' }}; \
             Start-Sleep -Milliseconds 250 }}; Get-Content -Raw -LiteralPath $path"
        )
    } else {
        let path = path.to_string_lossy();
        let path = path.replace('\'', "'\"'\"'");
        format!(
            "path='{path}'; version_path=\"$path.version\"; deadline=$((SECONDS + 120)); \
             while true; do version=-1; [ -f \"$version_path\" ] && version=$(cat \"$version_path\" 2>/dev/null || echo -1); \
             [ -f \"$path\" ] && [ \"$version\" -ge {required_version} ] && break; \
             [ \"$SECONDS\" -ge \"$deadline\" ] && {{ echo 'Timed out waiting for child conversation' >&2; exit 1; }}; \
             sleep 0.25; done; cat \"$path\""
        )
    }
}

// ── OpenAI tool_call → protobuf ToolCall ─────────────────────────────

fn openai_tool_call_to_proto(
    tc: &serde_json::Value,
    state: &AppState,
) -> Option<warp_multi_agent_api::message::ToolCall> {
    let fn_name = tc["function"]["name"].as_str()?;
    let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(json!({}));
    let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();

    use warp_multi_agent_api::message::tool_call::Tool;
    let tool = match fn_name {
        "run_shell_command" => {
            Tool::RunShellCommand(warp_multi_agent_api::message::tool_call::RunShellCommand {
                command: args["command"].as_str().unwrap_or("").into(),
                #[allow(deprecated)]
                is_read_only: args["is_read_only"].as_bool().unwrap_or(true),
                ..Default::default()
            })
        }
        "write_to_long_running_shell_command" => Tool::WriteToLongRunningShellCommand(
            warp_multi_agent_api::message::tool_call::WriteToLongRunningShellCommand {
                input: args["input"].as_str().unwrap_or("").as_bytes().to_vec(),
                mode: json_to_write_mode(args["mode"].as_str()),
                command_id: args["command_id"].as_str().unwrap_or("").into(),
            },
        ),
        "read_shell_command_output" => {
            let delay = if args["wait_for_completion"].as_bool().unwrap_or(false) {
                Some(warp_multi_agent_api::message::tool_call::read_shell_command_output::Delay::OnCompletion(()))
            } else {
                args["delay_seconds"].as_i64().map(|delay_seconds| {
                    warp_multi_agent_api::message::tool_call::read_shell_command_output::Delay::Duration(
                    prost_types::Duration {
                        seconds: delay_seconds,
                        nanos: 0,
                    },
                    )
                })
            };
            Tool::ReadShellCommandOutput(
                warp_multi_agent_api::message::tool_call::ReadShellCommandOutput {
                    command_id: args["command_id"].as_str().unwrap_or("").into(),
                    delay,
                },
            )
        }
        "transfer_shell_command_control_to_user" => Tool::TransferShellCommandControlToUser(
            warp_multi_agent_api::message::tool_call::TransferShellCommandControlToUser {
                reason: args["reason"].as_str().unwrap_or("").into(),
            },
        ),
        "read_files" => {
            let files = args["files"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|file| {
                            Some(warp_multi_agent_api::message::tool_call::read_files::File {
                                name: file["name"].as_str()?.into(),
                                line_ranges: json_to_line_ranges(&file["line_ranges"]),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::ReadFiles(warp_multi_agent_api::message::tool_call::ReadFiles { files })
        }
        "read_documents" => {
            let documents = args["documents"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|document| {
                            Some(warp_multi_agent_api::message::tool_call::read_documents::Document {
                                document_id: document["document_id"].as_str()?.into(),
                                line_ranges: json_to_line_ranges(&document["line_ranges"]),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::ReadDocuments(warp_multi_agent_api::message::tool_call::ReadDocuments {
                documents,
            })
        }
        "apply_file_diffs" => {
            let diffs = args["diffs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|diff| {
                            Some(warp_multi_agent_api::message::tool_call::apply_file_diffs::FileDiff {
                                file_path: diff["file_path"].as_str()?.into(),
                                search: diff["search"].as_str().unwrap_or("").into(),
                                replace: diff["replace"].as_str().unwrap_or("").into(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let new_files = args["new_files"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|file| {
                            Some(warp_multi_agent_api::message::tool_call::apply_file_diffs::NewFile {
                                file_path: file["file_path"].as_str()?.into(),
                                content: file["content"].as_str().unwrap_or("").into(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let deleted_files = args["deleted_files"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|file| {
                            Some(warp_multi_agent_api::message::tool_call::apply_file_diffs::DeleteFile {
                                file_path: file["file_path"].as_str()?.into(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let v4a_updates = args["v4a_updates"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|update| {
                            Some(warp_multi_agent_api::message::tool_call::apply_file_diffs::V4aFileUpdate {
                                file_path: update["file_path"].as_str()?.into(),
                                move_to: update["move_to"].as_str().unwrap_or("").into(),
                                hunks: update["hunks"]
                                    .as_array()
                                    .map(|hunks| {
                                        hunks
                                            .iter()
                                            .map(|hunk| {
                                                warp_multi_agent_api::message::tool_call::apply_file_diffs::v4a_file_update::Hunk {
                                                    change_context: json_string_array(&hunk["change_context"]),
                                                    pre_context: hunk["pre_context"].as_str().unwrap_or("").into(),
                                                    old: hunk["old"].as_str().unwrap_or("").into(),
                                                    new: hunk["new"].as_str().unwrap_or("").into(),
                                                    post_context: hunk["post_context"].as_str().unwrap_or("").into(),
                                                }
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::ApplyFileDiffs(warp_multi_agent_api::message::tool_call::ApplyFileDiffs {
                summary: args["summary"].as_str().unwrap_or("").into(),
                diffs,
                new_files,
                deleted_files,
                v4a_updates,
            })
        }
        "edit_documents" => {
            let diffs = args["diffs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|diff| {
                            Some(warp_multi_agent_api::message::tool_call::edit_documents::DocumentDiff {
                                document_id: diff["document_id"].as_str()?.into(),
                                search: diff["search"].as_str().unwrap_or("").into(),
                                replace: diff["replace"].as_str().unwrap_or("").into(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::EditDocuments(warp_multi_agent_api::message::tool_call::EditDocuments { diffs })
        }
        "create_documents" => {
            let new_documents = args["new_documents"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|document| warp_multi_agent_api::message::tool_call::create_documents::NewDocument {
                            content: document["content"].as_str().unwrap_or("").into(),
                            title: document["title"].as_str().unwrap_or("").into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::CreateDocuments(warp_multi_agent_api::message::tool_call::CreateDocuments {
                new_documents,
            })
        }
        "grep" => {
            let queries = {
                let queries = json_string_array(&args["queries"]);
                if queries.is_empty() {
                    args["pattern"]
                        .as_str()
                        .map(|pattern| vec![pattern.to_string()])
                        .unwrap_or_default()
                } else {
                    queries
                }
            };
            Tool::Grep(warp_multi_agent_api::message::tool_call::Grep {
                queries,
                path: args["path"].as_str().unwrap_or("").into(),
            })
        }
        "file_glob" => {
            let patterns = {
                let patterns = json_string_array(&args["patterns"]);
                if patterns.is_empty() {
                    args["pattern"]
                        .as_str()
                        .map(|pattern| vec![pattern.to_string()])
                        .unwrap_or_default()
                } else {
                    patterns
                }
            };
            Tool::FileGlobV2(warp_multi_agent_api::message::tool_call::FileGlobV2 {
                patterns,
                search_dir: args["search_dir"].as_str().unwrap_or("").into(),
                max_matches: args["max_matches"].as_i64().unwrap_or_default() as i32,
                max_depth: args["max_depth"].as_i64().unwrap_or_default() as i32,
                min_depth: args["min_depth"].as_i64().unwrap_or_default() as i32,
            })
        }
        "search_codebase" => {
            Tool::SearchCodebase(warp_multi_agent_api::message::tool_call::SearchCodebase {
                query: args["query"].as_str().unwrap_or("").into(),
                path_filters: json_string_array(&args["path_filters"]),
                codebase_path: args["codebase_path"].as_str().unwrap_or("").into(),
            })
        }
        "insert_review_comments" => {
            let comments = args["comments"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|comment| {
                            let location = comment.get("location").and_then(|location| {
                                let file_path = location["file_path"].as_str()?.to_string();
                                let line = location.get("line").map(|line| {
                                    let side = match line["side"].as_str() {
                                        Some("old") => {
                                            warp_multi_agent_api::message::tool_call::insert_review_comments::CommentSide::Old as i32
                                        }
                                        _ => warp_multi_agent_api::message::tool_call::insert_review_comments::CommentSide::New as i32,
                                    };
                                    warp_multi_agent_api::message::tool_call::insert_review_comments::CommentLineRange {
                                        diff_hunk: line["diff_hunk"].as_str().unwrap_or("").into(),
                                        range: line.get("range").and_then(|range| {
                                            Some(warp_multi_agent_api::FileContentLineRange {
                                                start: range.get("start")?.as_u64()? as u32,
                                                end: range.get("end")?.as_u64()? as u32,
                                            })
                                        }),
                                        side,
                                    }
                                });
                                Some(warp_multi_agent_api::message::tool_call::insert_review_comments::CommentLocation {
                                    file_path,
                                    line,
                                })
                            });

                            warp_multi_agent_api::message::tool_call::insert_review_comments::Comment {
                                comment_id: comment["comment_id"].as_str().unwrap_or("").into(),
                                author: comment["author"].as_str().unwrap_or("").into(),
                                last_modified_timestamp: comment["last_modified_timestamp"].as_str().unwrap_or("").into(),
                                comment_body: comment["comment_body"].as_str().unwrap_or("").into(),
                                parent_comment_id: comment["parent_comment_id"].as_str().unwrap_or("").into(),
                                location,
                                html_url: comment["html_url"].as_str().unwrap_or("").into(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::InsertReviewComments(
                warp_multi_agent_api::message::tool_call::InsertReviewComments {
                    repo_path: args["repo_path"].as_str().unwrap_or("").into(),
                    comments,
                    base_branch: args["base_branch"].as_str().unwrap_or("").into(),
                },
            )
        }
        "open_code_review" => {
            Tool::OpenCodeReview(warp_multi_agent_api::message::tool_call::OpenCodeReview {})
        }
        "suggest_plan" => {
            Tool::SuggestPlan(warp_multi_agent_api::message::tool_call::SuggestPlan {
                summary: args["summary"].as_str().unwrap_or("").into(),
                proposed_tasks: vec![],
            })
        }
        "suggest_create_plan" => {
            Tool::SuggestCreatePlan(warp_multi_agent_api::message::tool_call::SuggestCreatePlan {})
        }
        "ask_user_question" => {
            let questions = args["questions"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|question| {
                            let question_id = question["question_id"].as_str()?.to_string();
                            let multiple_choice = question.get("multiple_choice").map(|multiple_choice| {
                                warp_multi_agent_api::ask_user_question::question::QuestionType::MultipleChoice(
                                    warp_multi_agent_api::ask_user_question::MultipleChoice {
                                        options: json_string_array(&multiple_choice["options"])
                                            .into_iter()
                                            .map(|label| warp_multi_agent_api::ask_user_question::Option { label })
                                            .collect(),
                                        recommended_option_index: multiple_choice["recommended_option_index"]
                                            .as_i64()
                                            .unwrap_or_default()
                                            as i32,
                                        is_multiselect: multiple_choice["is_multiselect"].as_bool().unwrap_or(false),
                                        supports_other: multiple_choice["supports_other"].as_bool().unwrap_or(false),
                                    },
                                )
                            }).or_else(|| {
                                Some(
                                    warp_multi_agent_api::ask_user_question::question::QuestionType::MultipleChoice(
                                        warp_multi_agent_api::ask_user_question::MultipleChoice {
                                            options: vec![warp_multi_agent_api::ask_user_question::Option {
                                                label: "Enter a custom response".into(),
                                            }],
                                            recommended_option_index: 0,
                                            is_multiselect: false,
                                            supports_other: true,
                                        },
                                    ),
                                )
                            });
                            Some(warp_multi_agent_api::ask_user_question::Question {
                                question_id,
                                question: question["question"].as_str().unwrap_or("").into(),
                                question_type: multiple_choice,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::AskUserQuestion(warp_multi_agent_api::AskUserQuestion { questions })
        }
        "suggest_prompt" => {
            let display_mode = match args["display_mode"].as_str().unwrap_or("prompt_chip") {
                "inline_banner" => {
                    warp_multi_agent_api::message::tool_call::suggest_prompt::DisplayMode::InlineQueryBanner(
                        warp_multi_agent_api::message::tool_call::suggest_prompt::InlineQueryBanner {
                            title: args["title"].as_str().unwrap_or("").into(),
                            description: args["description"].as_str().unwrap_or("").into(),
                            query: args["text"].as_str().unwrap_or("").into(),
                        },
                    )
                }
                _ => {
                    warp_multi_agent_api::message::tool_call::suggest_prompt::DisplayMode::PromptChip(
                        warp_multi_agent_api::message::tool_call::suggest_prompt::PromptChip {
                            prompt: args["text"].as_str().unwrap_or("").into(),
                            label: args["label"].as_str().unwrap_or("").into(),
                        },
                    )
                }
            };
            Tool::SuggestPrompt(warp_multi_agent_api::message::tool_call::SuggestPrompt {
                display_mode: Some(display_mode),
                ..Default::default()
            })
        }
        "read_skill" => {
            let skill_reference = args["skill_path"]
                .as_str()
                .map(|path| warp_multi_agent_api::message::tool_call::read_skill::SkillReference::SkillPath(path.into()))
                .or_else(|| {
                    args["bundled_skill_id"].as_str().map(|id| {
                        warp_multi_agent_api::message::tool_call::read_skill::SkillReference::BundledSkillId(id.into())
                    })
                });
            Tool::ReadSkill(warp_multi_agent_api::message::tool_call::ReadSkill {
                skill_reference,
                name: args["name"].as_str().unwrap_or("").into(),
            })
        }
        "call_mcp_tool" => {
            let mcp_args = if args["args"].is_object() {
                json_to_prost_struct(&args["args"])
            } else {
                args["args"]
                    .as_str()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .as_ref()
                    .and_then(json_to_prost_struct)
            };
            Tool::CallMcpTool(warp_multi_agent_api::message::tool_call::CallMcpTool {
                name: args["name"].as_str().unwrap_or("").into(),
                args: mcp_args,
                server_id: args["server_id"].as_str().unwrap_or("").into(),
            })
        }
        "read_mcp_resource" => {
            Tool::ReadMcpResource(warp_multi_agent_api::message::tool_call::ReadMcpResource {
                uri: args["uri"].as_str().unwrap_or("").into(),
                server_id: args["server_id"].as_str().unwrap_or("").into(),
            })
        }
        "init_project" => {
            Tool::InitProject(warp_multi_agent_api::message::tool_call::InitProject {})
        }
        "use_computer" => {
            use warp_multi_agent_api::message::tool_call::use_computer::{action, Action};
            let actions = args["actions"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| {
                            let action_type = a["type"].as_str()?;
                            let x = a["x"].as_i64().unwrap_or(0) as i32;
                            let y = a["y"].as_i64().unwrap_or(0) as i32;
                            let coords = warp_multi_agent_api::Coordinates { x, y };
                            let t = match action_type {
                                "mouse_move" => {
                                    action::Type::MouseMove(action::MouseMove { to: Some(coords) })
                                }
                                "mouse_down" => action::Type::MouseDown(action::MouseDown {
                                    button: match a["button"].as_str().unwrap_or("left") {
                                        "right" => 1,
                                        "middle" => 2,
                                        _ => 0,
                                    },
                                    at: Some(coords),
                                }),
                                "mouse_up" => action::Type::MouseUp(action::MouseUp {
                                    button: match a["button"].as_str().unwrap_or("left") {
                                        "right" => 1,
                                        "middle" => 2,
                                        _ => 0,
                                    },
                                }),
                                "type_text" => action::Type::TypeText(action::TypeText {
                                    text: a["text"].as_str().unwrap_or("").into(),
                                }),
                                "key_down" => action::Type::KeyDown(action::KeyDown {
                                    key: a["key"].as_str().map(|k| action::Key {
                                        data: Some(action::key::Data::Char(k.into())),
                                    }),
                                }),
                                "key_up" => action::Type::KeyUp(action::KeyUp {
                                    key: a["key"].as_str().map(|k| action::Key {
                                        data: Some(action::key::Data::Char(k.into())),
                                    }),
                                }),
                                "wait" => action::Type::Wait(action::Wait {
                                    duration: Some(prost_types::Duration {
                                        seconds: a["duration_ms"].as_i64().unwrap_or(1000) / 1000,
                                        nanos: ((a["duration_ms"].as_i64().unwrap_or(1000) % 1000)
                                            * 1_000_000)
                                            as i32,
                                    }),
                                }),
                                _ => return None,
                            };
                            Some(Action {
                                target: None,
                                r#type: Some(t),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::UseComputer(warp_multi_agent_api::message::tool_call::UseComputer {
                actions,
                action_summary: args["action_summary"].as_str().unwrap_or("").into(),
                ..Default::default()
            })
        }
        "request_computer_use" => Tool::RequestComputerUse(
            warp_multi_agent_api::message::tool_call::RequestComputerUse {
                task_summary: args["task_summary"].as_str().unwrap_or("").into(),
                ..Default::default()
            },
        ),
        "subagent" => {
            let name = args["task_id"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or("subagent")
                .to_string();
            Tool::RunAgents(warp_multi_agent_api::RunAgents {
                summary: format!("Start local subagent {name}"),
                agent_run_configs: vec![warp_multi_agent_api::run_agents::AgentRunConfig {
                    name: name.clone(),
                    prompt: args["payload"].as_str().unwrap_or("").into(),
                    title: name,
                    agent_identity_uid: String::new(),
                    model_id: String::new(),
                    harness: None,
                    execution_mode: None,
                }],
                execution_mode: Some(warp_multi_agent_api::run_agents::ExecutionModeOneOf::Local(
                    warp_multi_agent_api::run_agents::Local {},
                )),
                harness: Some(oz_harness()),
                ..Default::default()
            })
        }
        "start_agent" => {
            let name = args["name"].as_str().unwrap_or("").to_owned();
            Tool::RunAgents(warp_multi_agent_api::RunAgents {
                summary: format!("Start agent {name}"),
                agent_run_configs: vec![warp_multi_agent_api::run_agents::AgentRunConfig {
                    name: name.clone(),
                    prompt: args["prompt"].as_str().unwrap_or("").into(),
                    title: name,
                    agent_identity_uid: String::new(),
                    model_id: String::new(),
                    harness: None,
                    execution_mode: None,
                }],
                execution_mode: Some(warp_multi_agent_api::run_agents::ExecutionModeOneOf::Local(
                    warp_multi_agent_api::run_agents::Local {},
                )),
                harness: Some(oz_harness()),
                ..Default::default()
            })
        }
        "send_message_to_agent" => {
            Tool::SendMessageToAgent(warp_multi_agent_api::SendMessageToAgent {
                addresses: json_string_array(&args["addresses"]),
                subject: args["subject"].as_str().unwrap_or("").into(),
                message: args["message"].as_str().unwrap_or("").into(),
            })
        }
        "wait_for_events" => {
            Tool::WaitForEvents(warp_multi_agent_api::message::tool_call::WaitForEvents {
                idle_timeout_seconds: args["idle_timeout_seconds"]
                    .as_i64()
                    .unwrap_or(30)
                    .clamp(1, 300) as i32,
            })
        }
        "fetch_conversation" => {
            let conversation_id = args["conversation_id"].as_str().unwrap_or("");
            if let Some((path, required_version)) = state.conversation_cache_target(conversation_id)
            {
                Tool::RunShellCommand(warp_multi_agent_api::message::tool_call::RunShellCommand {
                    command: wait_for_conversation_command(&path, required_version),
                    #[allow(deprecated)]
                    is_read_only: true,
                    ..Default::default()
                })
            } else {
                Tool::FetchConversation(
                    warp_multi_agent_api::message::tool_call::FetchConversation {
                        conversation_id: conversation_id.into(),
                    },
                )
            }
        }
        "upload_file_artifact" => {
            Tool::UploadFileArtifact(warp_multi_agent_api::UploadFileArtifact {
                file: Some(warp_multi_agent_api::FilePathReference {
                    file_path: args["file_path"].as_str().unwrap_or("").into(),
                }),
                description: args["description"].as_str().unwrap_or("").into(),
            })
        }
        "run_agents" => {
            let agent_run_configs = args["agents"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|agent| {
                            Some(warp_multi_agent_api::run_agents::AgentRunConfig {
                                name: agent["name"].as_str()?.into(),
                                prompt: agent["prompt"].as_str().unwrap_or("").into(),
                                title: agent["title"].as_str().unwrap_or("").into(),
                                agent_identity_uid: agent["agent_identity_uid"]
                                    .as_str()
                                    .unwrap_or("")
                                    .into(),
                                model_id: agent["model_id"].as_str().unwrap_or("").into(),
                                harness: None,
                                execution_mode: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Tool::RunAgents(warp_multi_agent_api::RunAgents {
                summary: args["summary"].as_str().unwrap_or("").into(),
                base_prompt: args["base_prompt"].as_str().unwrap_or("").into(),
                agent_run_configs,
                execution_mode: Some(warp_multi_agent_api::run_agents::ExecutionModeOneOf::Local(
                    warp_multi_agent_api::run_agents::Local {},
                )),
                harness: Some(oz_harness()),
                ..Default::default()
            })
        }
        _ => return None,
    };

    Some(warp_multi_agent_api::message::ToolCall {
        tool_call_id,
        tool: Some(tool),
    })
}

// ── Main handler ─────────────────────────────────────────────────────

fn resolve_backend(
    request: &warp_multi_agent_api::Request,
    selected_model: &str,
    fallback: &Config,
) -> anyhow::Result<(Config, String)> {
    use warp_multi_agent_api::request::settings::custom_model_providers::CustomEndpointSchema;

    let Some(providers) = request
        .settings
        .as_ref()
        .and_then(|settings| settings.custom_model_providers.as_ref())
    else {
        return Ok((fallback.clone(), selected_model.to_string()));
    };

    for provider in &providers.providers {
        let Some(model) = provider
            .models
            .iter()
            .find(|model| model.config_key == selected_model)
        else {
            continue;
        };

        let schema = CustomEndpointSchema::try_from(provider.schema)
            .map_err(|_| anyhow::anyhow!("custom model provider has an unknown schema"))?;
        if schema != CustomEndpointSchema::OpenaiChatCompletions {
            anyhow::bail!(
                "custom model '{}' uses unsupported schema {}; warp_local_proxy currently supports OpenAI Chat Completions",
                model.slug,
                schema.as_str_name()
            );
        }

        let mut config = fallback.clone();
        config.backend_base_url = provider.base_url.clone();
        config.backend_auth_style = AuthStyle::Bearer;
        config.backend_api_key = (!provider.api_key.is_empty()).then(|| provider.api_key.clone());
        config.azure_api_version = None;
        config.default_model = model.slug.clone();
        return Ok((config, model.slug.clone()));
    }

    Ok((fallback.clone(), selected_model.to_string()))
}

fn terminal_result_without_explicit_parent_message<'a>(
    messages: &'a [serde_json::Value],
    parent_agent_id: &str,
) -> Option<&'a str> {
    let last = messages.last()?;
    if last.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    let content = last
        .get("content")
        .and_then(serde_json::Value::as_str)
        .filter(|content| !content.trim().is_empty())?;
    let current_turn_start = messages
        .iter()
        .rposition(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("user")
        })
        .unwrap_or_default();
    let explicitly_messaged_parent = messages[current_turn_start + 1..].iter().any(|message| {
        message
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tool_calls| {
                tool_calls.iter().any(|tool_call| {
                    if tool_call
                        .pointer("/function/name")
                        .and_then(serde_json::Value::as_str)
                        != Some("send_message_to_agent")
                    {
                        return false;
                    }
                    tool_call
                        .pointer("/function/arguments")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|arguments| {
                            serde_json::from_str::<serde_json::Value>(arguments).ok()
                        })
                        .and_then(|arguments| {
                            arguments
                                .get("addresses")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                        })
                        .is_some_and(|addresses| {
                            addresses
                                .iter()
                                .any(|address| address.as_str() == Some(parent_agent_id))
                        })
                })
            })
    });
    (!explicitly_messaged_parent).then_some(content)
}

fn forward_terminal_child_result(
    state: &AppState,
    parent_agent_id: Option<&str>,
    agent_name: Option<&str>,
    task_id: &str,
    messages: &[serde_json::Value],
) {
    let Some(parent_agent_id) = parent_agent_id.filter(|id| !id.is_empty()) else {
        return;
    };
    let Some(result) = terminal_result_without_explicit_parent_message(messages, parent_agent_id)
    else {
        return;
    };
    let agent_name = agent_name
        .filter(|name| !name.is_empty())
        .unwrap_or("Child agent");
    if state
        .send_agent_message(
            parent_agent_id,
            task_id,
            &format!("{agent_name} result"),
            result,
        )
        .is_none()
    {
        tracing::warn!(
            parent_agent_id,
            task_id,
            "could not forward terminal child result"
        );
    }
}

pub async fn handle(State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
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
    let available_tools = openai_tools_for_request(&request);

    let user_query = extract_user_query(&request);
    register_launched_agents(&request, &state);
    let tool_results = extract_tool_results(&request, &state);
    let received_agent_messages = extract_received_agent_messages(&request);

    // Debug: log the input type to diagnose tool result delivery
    if let Some(input) = request.input.as_ref() {
        if let Some(ref input_type) = input.r#type {
            tracing::info!(input_type = ?std::mem::discriminant(input_type), "request input type");
        }
    }

    // Extract the model selected in the UI (from request.settings.model_config.base).
    // Fall back to the proxy's default model if not specified.
    let selected_model = request
        .settings
        .as_ref()
        .and_then(|s| s.model_config.as_ref())
        .map(|mc| mc.base.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.config.default_model.clone());
    let (backend_config, backend_model) =
        match resolve_backend(&request, &selected_model, &state.config) {
            Ok(resolved) => resolved,
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from(error.to_string()))
                    .unwrap();
            }
        };

    // Detect if this is a continuation by checking for existing tasks or tool results
    let existing_task_id = request
        .task_context
        .as_ref()
        .and_then(|tc| tc.tasks.first())
        .map(|t| t.id.clone());
    let is_continuation = existing_task_id.is_some();

    tracing::info!(
        query = user_query.as_deref().unwrap_or("(none)"),
        tool_results = tool_results.len(),
        received_agent_messages = received_agent_messages.len(),
        is_continuation,
        existing_task = existing_task_id.as_deref().unwrap_or("(none)"),
        model = %selected_model,
        backend_model = %backend_model,
        "multi-agent request"
    );

    // Task/run identity is stable across every turn. Agent messaging and event
    // streams address runs, so changing this per request silently disconnects
    // parent and child streams.
    let task_id = existing_task_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let (conversation_id, run_id) = stream_identity(&request, &task_id);
    let request_id = uuid::Uuid::new_v4().to_string();
    let parent_agent_id = request
        .metadata
        .as_ref()
        .map(|metadata| metadata.parent_agent_id.clone())
        .filter(|id| !id.is_empty());
    let agent_name = request
        .metadata
        .as_ref()
        .map(|metadata| metadata.agent_name.clone())
        .filter(|name| !name.is_empty());
    state.register_conversation_task(&conversation_id, &task_id);
    if let Some(metadata) = request.metadata.as_ref() {
        state.register_conversation_task(&metadata.conversation_id, &task_id);
        state.register_agent_task(&metadata.agent_name, &task_id);
        state.register_agent_address(&metadata.parent_agent_id);
    }

    // ── Server-side conversation cache ──
    // The Warp client does NOT persist ToolCallResults or follow-up user
    // queries back into task_context messages. Without caching, the LLM
    // loses memory of what it already did and loops endlessly.
    //
    // We maintain a per-task Vec<serde_json::Value> of OpenAI messages
    // and only APPEND new inputs each turn.
    let mut openai_messages = state.load_conversation(&task_id);

    // If this is a brand new conversation, add the system prompt
    if openai_messages.is_empty() {
        let parent_agent_id = request
            .metadata
            .as_ref()
            .map(|metadata| metadata.parent_agent_id.as_str())
            .filter(|parent_agent_id| !parent_agent_id.is_empty());
        openai_messages.push(json!({
            "role": "system",
            "content": system_prompt(parent_agent_id)
        }));
    }

    // Handle SummarizeConversation: ask the LLM to summarize, replace cache
    if is_summarize_request(&request) {
        tracing::info!("summarize request — compacting conversation");
        let summary_prompt = json!([
            { "role": "system", "content": "Summarize the following conversation into a concise recap. Preserve key facts, decisions, file paths, and tool results. Output only the summary." },
            { "role": "user", "content": openai_messages.iter()
                .filter(|m| m["role"] != "system")
                .map(|m| {
                    let role = m["role"].as_str().unwrap_or("?");
                    let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let tc = m.get("tool_calls").map(|t| format!("[tool_calls: {}]", t));
                    format!("[{role}] {}{}", content, tc.unwrap_or_default())
                })
                .collect::<Vec<_>>()
                .join("\n")
            }
        ]);
        let summary_messages: Vec<serde_json::Value> =
            serde_json::from_value(summary_prompt).unwrap_or_default();
        let summary_result = call_backend_with_tools(
            &state,
            &backend_config,
            &summary_messages,
            false,
            &backend_model,
            &available_tools,
        )
        .await;
        if let Ok(LlmResult {
            response: LlmResponse::Text(ref summary),
            ..
        }) = summary_result
        {
            // Replace conversation with system + summary
            openai_messages = vec![
                openai_messages[0].clone(), // keep system prompt
                json!({ "role": "assistant", "content": format!("[Conversation summary]\n{summary}") }),
            ];
            state.save_conversation(&task_id, &openai_messages);

            // Emit a Summarization message so the UI shows it
            let mut sse_body = String::new();
            sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
                r#type: Some(warp_multi_agent_api::response_event::Type::Init(
                    warp_multi_agent_api::response_event::StreamInit {
                        conversation_id: conversation_id.clone(),
                        request_id: request_id.clone(),
                        run_id: run_id.clone(),
                    },
                )),
            }));
            emit_agent_output(
                &mut sse_body,
                &task_id,
                &request_id,
                &format!("Conversation compacted. Summary:\n{summary}"),
            );
            sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
                r#type: Some(warp_multi_agent_api::response_event::Type::Finished(
                    warp_multi_agent_api::response_event::StreamFinished {
                        reason: Some(
                            warp_multi_agent_api::response_event::stream_finished::Reason::Done(
                                warp_multi_agent_api::response_event::stream_finished::Done {},
                            ),
                        ),
                        conversation_usage_metadata: Some(
                            warp_multi_agent_api::response_event::stream_finished::ConversationUsageMetadata {
                                context_window_usage: 0.05, // minimal after compaction
                                summarized: true,
                                ..Default::default()
                            },
                        ),
                        ..Default::default()
                    },
                )),
            }));
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(sse_body))
                .unwrap();
        }
    }

    // Add new user query (if present)
    if let Some(ref q) = user_query {
        if !q.is_empty() {
            openai_messages.push(json!({ "role": "user", "content": q }));
        }
    }

    // Add tool results from current request input
    for (tool_call_id, result_text) in &tool_results {
        openai_messages.push(json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": result_text
        }));
    }
    for message in &received_agent_messages {
        openai_messages.push(json!({
            "role": "user",
            "content": message
        }));
    }

    // Handle other input types as user messages
    if user_query.is_none()
        && tool_results.is_empty()
        && received_agent_messages.is_empty()
        && !is_summarize_request(&request)
    {
        if let Some(input) = request.input.as_ref().and_then(|i| i.r#type.as_ref()) {
            let extra_input = match input {
                warp_multi_agent_api::request::input::Type::ResumeConversation(_) => {
                    Some("(Conversation resumed)".to_string())
                }
                warp_multi_agent_api::request::input::Type::CodeReview(cr) => {
                    // Extract diff hunks from the CodeReview input
                    let mut review_text =
                        String::from("Please review the following code changes:\n\n");
                    if let Some(warp_multi_agent_api::request::input::code_review::Operation::InitialReviewComments(irc)) = &cr.operation {
                        if let Some(diff_set) = &irc.diff_set {
                            for hunk in &diff_set.hunks {
                                review_text.push_str(&format!("=== {} (lines +{} -{}) ===\n{}\n\n",
                                    hunk.file_path, hunk.lines_added, hunk.lines_removed, hunk.diff_content));
                            }
                        }
                        if !irc.review_comments.is_empty() {
                            review_text.push_str("Existing review comments:\n");
                            for c in &irc.review_comments {
                                review_text.push_str(&format!("- {}\n", c.comment));
                            }
                        }
                    }
                    Some(review_text)
                }
                warp_multi_agent_api::request::input::Type::AutoCodeDiffQuery(q) => {
                    Some(format!("Auto code diff: {}", q.query))
                }
                warp_multi_agent_api::request::input::Type::InvokeSkill(_s) => {
                    Some("Invoke skill.".to_string())
                }
                warp_multi_agent_api::request::input::Type::CreateNewProject(p) => {
                    Some(format!("Create new project: {}", p.query))
                }
                warp_multi_agent_api::request::input::Type::InitProjectRules(_) => {
                    Some("Initialize project rules.".to_string())
                }
                warp_multi_agent_api::request::input::Type::FetchReviewComments(fr) => {
                    Some(format!("Fetch review comments for: {}", fr.repo_path))
                }
                _ => None,
            };
            if let Some(msg) = extra_input {
                openai_messages.push(json!({ "role": "user", "content": msg }));
            }
        }
    }

    // Auto-summarize if context is getting large (>75% of estimated window)
    let estimated_tokens: usize = openai_messages
        .iter()
        .map(|m| m.to_string().len() / 4) // rough estimate: 4 chars per token
        .sum();
    const AUTO_SUMMARIZE_THRESHOLD: usize = 96_000; // 75% of 128k
    if estimated_tokens > AUTO_SUMMARIZE_THRESHOLD {
        tracing::info!(
            estimated_tokens,
            "auto-summarizing conversation (>75% context)"
        );
        let summary_text = openai_messages
            .iter()
            .filter(|m| m["role"] != "system")
            .map(|m| {
                let role = m["role"].as_str().unwrap_or("?");
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                format!("[{role}] {}", &content[..content.len().min(500)])
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary_msgs = vec![
            json!({ "role": "system", "content": "Summarize this conversation concisely. Keep key facts, file paths, decisions, and tool results." }),
            json!({ "role": "user", "content": summary_text }),
        ];
        if let Ok(LlmResult {
            response: LlmResponse::Text(ref summary),
            ..
        }) = call_backend_with_tools(
            &state,
            &backend_config,
            &summary_msgs,
            false,
            &backend_model,
            &available_tools,
        )
        .await
        {
            openai_messages = vec![
                openai_messages[0].clone(),
                json!({ "role": "assistant", "content": format!("[Auto-compacted summary]\n{summary}") }),
            ];
            state.save_conversation(&task_id, &openai_messages);
        }
    }

    // Patch orphaned tool_calls for the LLM call only (not persisted).
    // If a user typed a message before a tool result arrived, the cache has
    // [assistant(tool_call), user(msg)] with no tool result. OpenAI rejects
    // this. We create a patched copy with placeholders for the LLM, but keep
    // the real cache clean so the actual tool result can arrive later.
    let llm_messages = {
        let mut patched = openai_messages.clone();
        let mut i = 0;
        while i < patched.len() {
            if let Some(tcs) = patched[i].get("tool_calls").and_then(|v| v.as_array()) {
                let needed: std::collections::HashSet<String> = tcs
                    .iter()
                    .filter_map(|t| t["id"].as_str().map(String::from))
                    .collect();
                let mut found = std::collections::HashSet::new();
                let mut j = i + 1;
                while j < patched.len() && patched[j]["role"] == "tool" {
                    if let Some(id) = patched[j]["tool_call_id"].as_str() {
                        found.insert(id.to_string());
                    }
                    j += 1;
                }
                let missing: Vec<String> = needed.difference(&found).cloned().collect();
                for (offset, id) in missing.iter().enumerate() {
                    patched.insert(
                        j + offset,
                        json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": "(cancelled by user)"
                        }),
                    );
                }
            }
            i += 1;
        }
        patched
    };

    // Count tool call rounds for the max-rounds limit
    const MAX_TOOL_ROUNDS: u32 = 200;
    let prior_tool_rounds = llm_messages
        .iter()
        .filter(|m| m.get("tool_calls").is_some())
        .count() as u32;

    let send_tools = prior_tool_rounds < MAX_TOOL_ROUNDS;
    if !send_tools {
        tracing::warn!(
            prior_rounds = prior_tool_rounds,
            max = MAX_TOOL_ROUNDS,
            "max tool rounds reached, forcing text response"
        );
    }

    let llm_result = call_backend_streaming(
        &state,
        &backend_config,
        &llm_messages,
        send_tools,
        &backend_model,
        &available_tools,
    )
    .await;

    let mut sse_body = String::new();

    // StreamInit
    sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::Init(
            warp_multi_agent_api::response_event::StreamInit {
                conversation_id: conversation_id.clone(),
                request_id: request_id.clone(),
                run_id: run_id.clone(),
            },
        )),
    }));

    // CreateTask + echo user query (first turn only)
    if !is_continuation {
        sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
            r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
                warp_multi_agent_api::response_event::ClientActions {
                    actions: vec![warp_multi_agent_api::ClientAction {
                        action: Some(warp_multi_agent_api::client_action::Action::CreateTask(
                            warp_multi_agent_api::client_action::CreateTask {
                                task: Some(warp_multi_agent_api::Task {
                                    id: task_id.clone(),
                                    description: task_description(user_query.as_deref()),
                                    ..Default::default()
                                }),
                            },
                        )),
                    }],
                },
            )),
        }));

        if let Some(ref q) = user_query {
            let user_msg_id = uuid::Uuid::new_v4().to_string();
            sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
                r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
                    warp_multi_agent_api::response_event::ClientActions {
                        actions: vec![warp_multi_agent_api::ClientAction {
                            action: Some(
                                warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                                    warp_multi_agent_api::client_action::AddMessagesToTask {
                                        task_id: task_id.clone(),
                                        messages: vec![warp_multi_agent_api::Message {
                                            id: user_msg_id,
                                            task_id: task_id.clone(),
                                            request_id: request_id.clone(),
                                            timestamp: Some(current_timestamp()),
                                            message: Some(
                                                warp_multi_agent_api::message::Message::UserQuery(
                                                    warp_multi_agent_api::message::UserQuery {
                                                        query: q.clone(),
                                                        ..Default::default()
                                                    },
                                                ),
                                            ),
                                            ..Default::default()
                                        }],
                                    },
                                ),
                            ),
                        }],
                    },
                )),
            }));
        }
    }

    let context_usage = match llm_result {
        Ok(StreamingLlmResult::TextStream(text_rx)) => {
            let (output_tx, output_rx) = mpsc::channel(32);
            let _ = output_tx.send(Ok(Bytes::from(sse_body))).await;

            let agent_msg_id = uuid::Uuid::new_v4().to_string();
            let _ = send_sse_event(
                &output_tx,
                agent_output_placeholder_event(&task_id, &request_id, &agent_msg_id),
            )
            .await;

            let state = state.clone();
            let task_id = task_id.clone();
            let request_id = request_id.clone();
            let parent_agent_id = parent_agent_id.clone();
            let agent_name = agent_name.clone();
            tokio::spawn(async move {
                let mut text_rx = text_rx;
                let mut openai_messages = openai_messages;
                let mut accumulated = String::new();
                let mut client_closed = false;

                while let Some(chunk) = text_rx.recv().await {
                    accumulated.push_str(&chunk);
                    if !client_closed
                        && send_sse_event(
                            &output_tx,
                            agent_output_append_event(&task_id, &request_id, &agent_msg_id, &chunk),
                        )
                        .await
                        .is_err()
                    {
                        client_closed = true;
                    }
                }

                if accumulated.is_empty() {
                    accumulated.push_str("(no response)");
                    if !client_closed
                        && send_sse_event(
                            &output_tx,
                            agent_output_append_event(
                                &task_id,
                                &request_id,
                                &agent_msg_id,
                                "(no response)",
                            ),
                        )
                        .await
                        .is_err()
                    {
                        client_closed = true;
                    }
                }

                openai_messages.push(json!({ "role": "assistant", "content": accumulated }));
                state.save_conversation(&task_id, &openai_messages);
                forward_terminal_child_result(
                    &state,
                    parent_agent_id.as_deref(),
                    agent_name.as_deref(),
                    &task_id,
                    &openai_messages,
                );

                if !client_closed {
                    let context_usage = estimate_context_usage_from_messages(&openai_messages);
                    let _ = send_sse_event(&output_tx, finished_event(context_usage)).await;
                }
            });

            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from_stream(ReceiverStream::new(output_rx)))
                .unwrap();
        }
        Ok(StreamingLlmResult::ToolCalls(tool_calls, context_usage)) => {
            // Save ALL tool_calls in a SINGLE assistant message.
            // OpenAI requires all tool_calls from one response to be in one message.
            let tc_entries: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc["id"].as_str().unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": tc["function"]["name"].as_str().unwrap_or("unknown"),
                            "arguments": tc["function"]["arguments"].as_str().unwrap_or("{}")
                        }
                    })
                })
                .collect();
            openai_messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": tc_entries
            }));

            // Emit each tool call as a separate protobuf event for the client
            for tc in &tool_calls {
                if let Some(proto_tc) = openai_tool_call_to_proto(tc, &state) {
                    let tc_msg_id = uuid::Uuid::new_v4().to_string();
                    sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
                        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
                            warp_multi_agent_api::response_event::ClientActions {
                                actions: vec![warp_multi_agent_api::ClientAction {
                                    action: Some(
                                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                                task_id: task_id.clone(),
                                                messages: vec![warp_multi_agent_api::Message {
                                                    id: tc_msg_id,
                                                    task_id: task_id.clone(),
                                                    request_id: request_id.clone(),
                                                    timestamp: Some(current_timestamp()),
                                                    message: Some(
                                                        warp_multi_agent_api::message::Message::ToolCall(proto_tc),
                                                    ),
                                                    ..Default::default()
                                                }],
                                            },
                                        ),
                                    ),
                                }],
                            },
                        )),
                    }));
                }
            }
            context_usage
        }
        Err(e) => {
            tracing::error!("Backend call failed: {e}");
            openai_messages.push(json!({ "role": "assistant", "content": format!("Error: {e}") }));
            emit_agent_output(&mut sse_body, &task_id, &request_id, &format!("Error: {e}"));
            estimate_context_usage_from_messages(&openai_messages)
        }
    };

    // Persist conversation to disk
    state.save_conversation(&task_id, &openai_messages);

    sse_body.push_str(&sse_line(&finished_event(context_usage)));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(sse_body))
        .unwrap()
}

// ── Emit agent text output ───────────────────────────────────────────

fn agent_output_placeholder_event(
    task_id: &str,
    request_id: &str,
    msg_id: &str,
) -> warp_multi_agent_api::ResponseEvent {
    warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.to_string(),
                                messages: vec![warp_multi_agent_api::Message {
                                    id: msg_id.to_string(),
                                    task_id: task_id.to_string(),
                                    request_id: request_id.to_string(),
                                    timestamp: Some(current_timestamp()),
                                    message: Some(
                                        warp_multi_agent_api::message::Message::AgentOutput(
                                            warp_multi_agent_api::message::AgentOutput {
                                                text: String::new(),
                                            },
                                        ),
                                    ),
                                    ..Default::default()
                                }],
                            },
                        ),
                    ),
                }],
            },
        )),
    }
}

fn agent_output_append_event(
    task_id: &str,
    request_id: &str,
    msg_id: &str,
    text: &str,
) -> warp_multi_agent_api::ResponseEvent {
    warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AppendToMessageContent(
                            warp_multi_agent_api::client_action::AppendToMessageContent {
                                task_id: task_id.to_string(),
                                message: Some(warp_multi_agent_api::Message {
                                    id: msg_id.to_string(),
                                    task_id: task_id.to_string(),
                                    request_id: request_id.to_string(),
                                    timestamp: Some(current_timestamp()),
                                    message: Some(
                                        warp_multi_agent_api::message::Message::AgentOutput(
                                            warp_multi_agent_api::message::AgentOutput {
                                                text: text.to_string(),
                                            },
                                        ),
                                    ),
                                    ..Default::default()
                                }),
                                mask: Some(prost_types::FieldMask {
                                    paths: vec!["agent_output.text".into()],
                                }),
                            },
                        ),
                    ),
                }],
            },
        )),
    }
}

fn finished_event(context_usage: f32) -> warp_multi_agent_api::ResponseEvent {
    warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::Finished(
            warp_multi_agent_api::response_event::StreamFinished {
                reason: Some(
                    warp_multi_agent_api::response_event::stream_finished::Reason::Done(
                        warp_multi_agent_api::response_event::stream_finished::Done {},
                    ),
                ),
                conversation_usage_metadata: Some(
                    warp_multi_agent_api::response_event::stream_finished::ConversationUsageMetadata {
                        context_window_usage: context_usage,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        )),
    }
}

fn emit_agent_output(sse_body: &mut String, task_id: &str, request_id: &str, text: &str) {
    let msg_id = uuid::Uuid::new_v4().to_string();
    sse_body.push_str(&sse_line(&agent_output_placeholder_event(
        task_id, request_id, &msg_id,
    )));
    sse_body.push_str(&sse_line(&agent_output_append_event(
        task_id, request_id, &msg_id, text,
    )));
}

// ── LLM backend ──────────────────────────────────────────────────────

enum LlmResponse {
    Text(String),
    ToolCalls(()),
}

struct LlmResult {
    response: LlmResponse,
    /// Fraction of context window used (0.0–1.0), from usage data.
    _context_usage: f32,
}

enum StreamingLlmResult {
    TextStream(mpsc::Receiver<String>),
    ToolCalls(Vec<serde_json::Value>, f32),
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    tool_type: String,
    function_name: String,
    function_arguments: String,
}

#[derive(Default)]
struct StreamToolCallAccumulator {
    tool_calls: BTreeMap<usize, PartialToolCall>,
}

impl StreamToolCallAccumulator {
    fn push_chunk(&mut self, chunks: &[serde_json::Value]) {
        for chunk in chunks {
            let index = chunk["index"].as_u64().unwrap_or(0) as usize;
            let entry = self.tool_calls.entry(index).or_default();

            if let Some(id) = chunk["id"].as_str() {
                entry.id = id.to_string();
            }
            if let Some(tool_type) = chunk["type"].as_str() {
                entry.tool_type = tool_type.to_string();
            }
            if let Some(name) = chunk["function"]["name"].as_str() {
                entry.function_name = name.to_string();
            }
            if let Some(arguments) = chunk["function"]["arguments"].as_str() {
                entry.function_arguments.push_str(arguments);
            }
        }
    }

    fn into_tool_calls(self) -> Vec<serde_json::Value> {
        self.tool_calls
            .into_values()
            .map(|tool_call| {
                json!({
                    "id": tool_call.id,
                    "type": if tool_call.tool_type.is_empty() { "function" } else { &tool_call.tool_type },
                    "function": {
                        "name": tool_call.function_name,
                        "arguments": tool_call.function_arguments,
                    }
                })
            })
            .collect()
    }
}

enum BackendStreamDecision {
    Text,
    ToolCalls(Vec<serde_json::Value>),
}

fn estimate_context_usage_from_messages(messages: &[serde_json::Value]) -> f32 {
    let estimated_tokens: usize = messages
        .iter()
        .map(|message| message.to_string().len() / 4)
        .sum();
    (estimated_tokens as f32 / 128_000.0).min(1.0)
}

fn drain_sse_line(buffer: &mut Vec<u8>) -> Option<String> {
    let newline_pos = buffer.iter().position(|byte| *byte == b'\n')?;
    let mut line = buffer.drain(..=newline_pos).collect::<Vec<_>>();
    if matches!(line.last(), Some(b'\n')) {
        line.pop();
    }
    if matches!(line.last(), Some(b'\r')) {
        line.pop();
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

async fn send_sse_event(
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    event: warp_multi_agent_api::ResponseEvent,
) -> Result<(), ()> {
    tx.send(Ok(Bytes::from(sse_line(&event))))
        .await
        .map_err(|_| ())
}

async fn queue_text_chunk(
    chunk: String,
    text_tx: &mpsc::Sender<String>,
    decision_tx: &Option<oneshot::Sender<Result<BackendStreamDecision, anyhow::Error>>>,
    pending_text: &mut Vec<String>,
) -> bool {
    if decision_tx.is_some() {
        pending_text.push(chunk);
        true
    } else {
        text_tx.send(chunk).await.is_ok()
    }
}

async fn commit_text_stream(
    text_tx: &mpsc::Sender<String>,
    decision_tx: &mut Option<oneshot::Sender<Result<BackendStreamDecision, anyhow::Error>>>,
    pending_text: &mut Vec<String>,
) -> bool {
    if let Some(tx) = decision_tx.take() {
        if tx.send(Ok(BackendStreamDecision::Text)).is_err() {
            return false;
        }
    }
    for chunk in pending_text.drain(..) {
        if text_tx.send(chunk).await.is_err() {
            return false;
        }
    }
    true
}

async fn handle_backend_stream_line(
    line: &str,
    text_tx: &mpsc::Sender<String>,
    decision_tx: &mut Option<oneshot::Sender<Result<BackendStreamDecision, anyhow::Error>>>,
    pending_text: &mut Vec<String>,
    tool_calls: &mut StreamToolCallAccumulator,
    saw_tool_calls: &mut bool,
    in_reasoning: &mut bool,
) -> Result<bool, anyhow::Error> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(true);
    };
    let data = data.trim();

    if data.is_empty() {
        return Ok(true);
    }
    if data == "[DONE]" {
        return Ok(false);
    }

    let chunk: serde_json::Value = serde_json::from_str(data)?;
    let Some(choice) = chunk["choices"]
        .as_array()
        .and_then(|choices| choices.first())
    else {
        return Ok(true);
    };
    let delta = &choice["delta"];

    if let Some(tool_call_chunks) = delta["tool_calls"].as_array() {
        if decision_tx.is_none() {
            tracing::warn!(
                "backend emitted tool calls after text streaming started; ignoring them"
            );
            return Ok(true);
        }
        *saw_tool_calls = true;
        pending_text.clear();
        tool_calls.push_chunk(tool_call_chunks);
        return Ok(true);
    }

    // DeepSeek models emit thinking/reasoning tokens in "reasoning_content"
    // before the final answer in "content". Stream them to the UI wrapped
    // in a blockquote so the user can distinguish reasoning from the answer.
    if let Some(reasoning) = delta["reasoning_content"].as_str() {
        if !reasoning.is_empty() && !*saw_tool_calls {
            if !*in_reasoning {
                *in_reasoning = true;
                if !queue_text_chunk(
                    "<details><summary>💭 Thinking...</summary>\n\n".to_string(),
                    text_tx,
                    decision_tx,
                    pending_text,
                )
                .await
                {
                    return Ok(false);
                }
            }
            if !queue_text_chunk(reasoning.to_string(), text_tx, decision_tx, pending_text).await {
                return Ok(false);
            }
            return Ok(true);
        }
        // Empty reasoning_content — fall through to check "content" field
    }

    if let Some(content) = delta["content"].as_str() {
        if content.is_empty() {
            return Ok(true);
        }
        if *saw_tool_calls {
            // Content after tool_calls is unusual; skip it
            return Ok(true);
        }
        if *in_reasoning {
            *in_reasoning = false;
            if !queue_text_chunk(
                "\n\n</details>\n\n".to_string(),
                text_tx,
                decision_tx,
                pending_text,
            )
            .await
            {
                return Ok(false);
            }
        }
        if !queue_text_chunk(content.to_string(), text_tx, decision_tx, pending_text).await
            || !commit_text_stream(text_tx, decision_tx, pending_text).await
        {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn call_backend_streaming(
    state: &AppState,
    config: &Config,
    messages: &[serde_json::Value],
    send_tools: bool,
    model: &str,
    tools: &serde_json::Value,
) -> Result<StreamingLlmResult, anyhow::Error> {
    let url = config.chat_completions_url_for_model(model);

    // Newer models (gpt-5.x, o-series) require max_completion_tokens;
    // older models (gpt-4o, DeepSeek, etc.) use max_tokens.
    let max_tokens_key = if model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };

    let mut payload = json!({
        "model": model,
        "messages": messages,
        max_tokens_key: 16384,
        "stream": true
    });

    if send_tools {
        payload["tools"] = tools.clone();
    }

    let mut req = state
        .http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&payload);
    req = apply_backend_auth(req, config);

    let resp = req.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Backend returned {status}: {body}");
    }

    let estimated_context_usage = estimate_context_usage_from_messages(messages);
    // Large buffer so text can be buffered while we wait for the stream to
    // finish before committing the Text-vs-ToolCalls decision.
    let (text_tx, text_rx) = mpsc::channel(4096);
    let (decision_tx, decision_rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut resp = resp;
        let mut decision_tx = Some(decision_tx);
        let mut buffer = Vec::new();
        let mut pending_text = Vec::new();
        let mut tool_calls = StreamToolCallAccumulator::default();
        let mut saw_tool_calls = false;
        let mut in_reasoning = false;

        let read_result: Result<(), anyhow::Error> = async {
            while let Some(chunk) = resp.chunk().await? {
                buffer.extend_from_slice(&chunk);
                while let Some(line) = drain_sse_line(&mut buffer) {
                    if !handle_backend_stream_line(
                        &line,
                        &text_tx,
                        &mut decision_tx,
                        &mut pending_text,
                        &mut tool_calls,
                        &mut saw_tool_calls,
                        &mut in_reasoning,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }

            if !buffer.is_empty()
                && !handle_backend_stream_line(
                    &String::from_utf8_lossy(&buffer),
                    &text_tx,
                    &mut decision_tx,
                    &mut pending_text,
                    &mut tool_calls,
                    &mut saw_tool_calls,
                    &mut in_reasoning,
                )
                .await?
            {
                return Ok(());
            }

            Ok(())
        }
        .await;

        match read_result {
            Ok(()) => {
                if saw_tool_calls {
                    if let Some(tx) = decision_tx.take() {
                        let _ = tx.send(Ok(BackendStreamDecision::ToolCalls(
                            tool_calls.into_tool_calls(),
                        )));
                    }
                } else {
                    if in_reasoning
                        && !queue_text_chunk(
                            "\n\n</details>\n\n".to_string(),
                            &text_tx,
                            &decision_tx,
                            &mut pending_text,
                        )
                        .await
                    {
                        return;
                    }
                    let _ = commit_text_stream(&text_tx, &mut decision_tx, &mut pending_text).await;
                }
            }
            Err(err) => {
                if let Some(tx) = decision_tx.take() {
                    let _ = tx.send(Err(err));
                } else {
                    tracing::error!(error = %err, "backend stream failed after text streaming started");
                }
            }
        }
    });

    match decision_rx.await {
        Ok(Ok(BackendStreamDecision::Text)) => Ok(StreamingLlmResult::TextStream(text_rx)),
        Ok(Ok(BackendStreamDecision::ToolCalls(tool_calls))) => Ok(StreamingLlmResult::ToolCalls(
            tool_calls,
            estimated_context_usage,
        )),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(anyhow::anyhow!(
            "backend stream ended before mode was determined"
        )),
    }
}

async fn call_backend_with_tools(
    state: &AppState,
    config: &Config,
    messages: &[serde_json::Value],
    send_tools: bool,
    model: &str,
    tools: &serde_json::Value,
) -> Result<LlmResult, anyhow::Error> {
    let url = config.chat_completions_url_for_model(model);

    let max_tokens_key = if model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };

    let mut payload = json!({
        "model": model,
        "messages": messages,
        max_tokens_key: 16384,
        "stream": false
    });

    if send_tools {
        payload["tools"] = tools.clone();
    }

    let mut req = state
        .http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&payload);
    req = apply_backend_auth(req, config);

    let resp = req.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Backend returned {status}: {body}");
    }

    let response_json: serde_json::Value = resp.json().await?;
    let choice = &response_json["choices"][0];
    let message = &choice["message"];

    // Estimate context window usage from token counts
    let prompt_tokens = response_json["usage"]["prompt_tokens"]
        .as_f64()
        .unwrap_or(0.0);
    let total_tokens = response_json["usage"]["total_tokens"]
        .as_f64()
        .unwrap_or(0.0);
    // Common context windows: 128k for most models, use total_tokens/128000 as estimate
    let context_limit = 128_000.0_f64;
    let context_usage = (total_tokens / context_limit).min(1.0) as f32;

    if let Some(tool_calls) = message["tool_calls"].as_array() {
        if !tool_calls.is_empty() {
            tracing::info!(
                count = tool_calls.len(),
                prompt_tokens,
                "LLM requested tool calls"
            );
            return Ok(LlmResult {
                response: LlmResponse::ToolCalls(()),
                _context_usage: context_usage,
            });
        }
    }

    let text = message["content"]
        .as_str()
        .unwrap_or("(no response)")
        .to_string();
    Ok(LlmResult {
        response: LlmResponse::Text(text),
        _context_usage: context_usage,
    })
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    fn fallback_config() -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            backend_base_url: "http://127.0.0.1:3113/v1".into(),
            backend_auth_style: AuthStyle::Bearer,
            backend_api_key: None,
            azure_api_version: None,
            default_model: "fallback-model".into(),
        }
    }

    #[test]
    fn generated_messages_use_current_timestamps_and_prompt_titles() {
        assert!(current_timestamp().seconds > 1_700_000_000);
        assert_eq!(
            task_description(Some("  Investigate the proxy  ")),
            "Investigate the proxy"
        );
        assert_eq!(task_description(Some("")), "Local agent");
        assert_eq!(task_description(Some(&"x".repeat(100))).len(), 80);
        let child_prompt = system_prompt(Some("parent-run-id"));
        assert!(child_prompt.contains("Multi-level orchestration is unavailable"));
        assert!(child_prompt.contains("never creates one"));
        assert!(child_prompt.contains("parent-run-id"));
        assert!(child_prompt.contains("delivered automatically"));
    }

    #[test]
    fn stream_identity_reuses_conversation_and_task_run_ids() {
        let request = warp_multi_agent_api::Request {
            metadata: Some(warp_multi_agent_api::request::Metadata {
                conversation_id: "conversation-1".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (conversation_id, run_id) = stream_identity(&request, "task-1");
        assert_eq!(conversation_id, "conversation-1");
        assert_eq!(run_id, "task-1");
    }

    #[test]
    fn run_agents_result_exposes_stable_child_address() {
        use warp_multi_agent_api::request;
        use warp_multi_agent_api::run_agents_result::{
            agent_outcome, AgentOutcome, Launched, LaunchedAgent, Outcome,
        };

        let state = AppState::new(fallback_config(), vec![]);
        state.register_launched_agent("child", "child-conversation-id");
        state.register_agent_task("child", "child-run-id");
        let result = request::input::ToolCallResult {
            tool_call_id: "run-agents-1".into(),
            result: Some(request::input::tool_call_result::Result::RunAgentsResult(
                warp_multi_agent_api::RunAgentsResult {
                    outcome: Some(Outcome::Launched(Launched {
                        agents: vec![AgentOutcome {
                            name: "child".into(),
                            result: Some(agent_outcome::Result::Launched(LaunchedAgent {
                                agent_id: "child-conversation-id".into(),
                            })),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })),
                },
            )),
        };

        let text = request_tool_call_result_to_text(&result, &state);
        assert!(text.contains("child-run-id"));
        assert!(text.contains("agent_address"));
        assert!(!text.contains("fetch_conversation"));
    }

    #[test]
    fn any_file_read_result_preserves_text_content() {
        use warp_multi_agent_api::any_file_content::Content;
        use warp_multi_agent_api::{read_files_result, request};

        let result = request::input::ToolCallResult {
            tool_call_id: "read-child-cache".into(),
            result: Some(request::input::tool_call_result::Result::ReadFiles(
                warp_multi_agent_api::ReadFilesResult {
                    result: Some(read_files_result::Result::AnyFilesSuccess(
                        read_files_result::AnyFilesSuccess {
                            files: vec![warp_multi_agent_api::AnyFileContent {
                                content: Some(Content::TextContent(
                                    warp_multi_agent_api::FileContent {
                                        file_path: "child.json".into(),
                                        content: r#"[{"content":"E2E_CHILD_OK"}]"#.into(),
                                        line_range: None,
                                    },
                                )),
                            }],
                            failed_reads: vec![],
                        },
                    )),
                },
            )),
        };

        let state = AppState::new(fallback_config(), vec![]);
        assert!(request_tool_call_result_to_text(&result, &state).contains("E2E_CHILD_OK"));
    }

    #[test]
    fn conversation_wait_command_reads_the_resolved_cache_file() {
        let path = std::path::Path::new("child-conversation.json");
        let command = wait_for_conversation_command(path, 7);
        assert!(command.contains("child-conversation.json"));
        assert!(command.contains("120"));
        assert!(command.contains('7'));
    }

    #[test]
    fn advertised_agent_tools_only_use_current_run_agents_contract() {
        let tools = openai_tools();
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(|name| name.as_str())
            })
            .collect::<Vec<_>>();
        assert!(names.contains(&"run_agents"));
        assert!(names.contains(&"send_message_to_agent"));
        assert!(!names.contains(&"fetch_conversation"));
        assert!(!names.contains(&"subagent"));
        assert!(!names.contains(&"start_agent"));
    }

    #[test]
    fn request_capabilities_filter_the_advertised_tool_catalog() {
        let request = warp_multi_agent_api::Request {
            settings: Some(warp_multi_agent_api::request::Settings {
                supported_tools: vec![warp_multi_agent_api::ToolType::RunAgents as i32],
                ..Default::default()
            }),
            ..Default::default()
        };
        let tools = openai_tools_for_request(&request);
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(|name| name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["run_agents"]);
    }

    #[test]
    fn wait_for_events_is_advertised_when_supported() {
        let request = warp_multi_agent_api::Request {
            settings: Some(warp_multi_agent_api::request::Settings {
                supported_tools: vec![warp_multi_agent_api::ToolType::WaitForEvents as i32],
                ..Default::default()
            }),
            ..Default::default()
        };
        let tools = openai_tools_for_request(&request);
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(|name| name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["wait_for_events"]);
    }

    #[test]
    fn child_requests_honor_the_client_supplied_tool_policy() {
        let request = warp_multi_agent_api::Request {
            settings: Some(warp_multi_agent_api::request::Settings {
                supported_tools: vec![
                    warp_multi_agent_api::ToolType::RunAgents as i32,
                    warp_multi_agent_api::ToolType::RunShellCommand as i32,
                ],
                ..Default::default()
            }),
            metadata: Some(warp_multi_agent_api::request::Metadata {
                parent_agent_id: "parent-run-id".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let tools = openai_tools_for_request(&request);
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(|name| name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["run_shell_command"]);
    }

    #[test]
    fn open_question_falls_back_to_freeform_capable_multiple_choice() {
        use warp_multi_agent_api::message::tool_call::Tool;

        let state = AppState::new(fallback_config(), vec![]);
        let tool_call = json!({
            "id": "question-1",
            "function": {
                "name": "ask_user_question",
                "arguments": serde_json::to_string(&json!({
                    "questions": [{
                        "question_id": "topic",
                        "question": "What should the diagram show?"
                    }]
                })).unwrap()
            }
        });

        let converted = openai_tool_call_to_proto(&tool_call, &state).unwrap();
        let Some(Tool::AskUserQuestion(question)) = converted.tool else {
            panic!("expected AskUserQuestion");
        };
        assert_eq!(question.questions.len(), 1);
        let question_type = question.questions[0].question_type.as_ref();
        assert!(matches!(
            question_type,
            Some(
                warp_multi_agent_api::ask_user_question::question::QuestionType::MultipleChoice(
                    multiple_choice
                )
            ) if multiple_choice.supports_other
        ));
    }

    #[test]
    fn received_agent_messages_are_forwarded_to_model_context() {
        use warp_multi_agent_api::request;
        use warp_multi_agent_api::request::input::user_inputs::user_input::Input;

        let request = warp_multi_agent_api::Request {
            input: Some(request::Input {
                r#type: Some(request::input::Type::UserInputs(
                    request::input::UserInputs {
                        inputs: vec![request::input::user_inputs::UserInput {
                            input: Some(Input::MessagesReceivedFromAgents(
                                request::input::user_inputs::MessagesReceivedFromAgents {
                                    messages: vec![
                                        request::input::user_inputs::messages_received_from_agents::ReceivedMessage {
                                            message_id: "message-1".into(),
                                            sender_agent_id: "child-1".into(),
                                            addresses: vec!["parent-1".into()],
                                            subject: "Done".into(),
                                            message_body: "CHILD_REPLY_OK".into(),
                                        },
                                    ],
                                },
                            )),
                        }],
                    },
                )),
                context: None,
            }),
            ..Default::default()
        };

        let messages = extract_received_agent_messages(&request);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("child-1"));
        assert!(messages[0].contains("CHILD_REPLY_OK"));
    }

    #[test]
    fn terminal_child_results_are_forwarded_only_without_an_explicit_message() {
        let implicit = vec![
            json!({"role": "user", "content": "Compute 2 + 2"}),
            json!({"role": "assistant", "content": "4"}),
        ];
        assert_eq!(
            terminal_result_without_explicit_parent_message(&implicit, "parent"),
            Some("4")
        );

        let explicit = vec![
            json!({"role": "user", "content": "Report back"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "function": {
                        "name": "send_message_to_agent",
                        "arguments": "{\"addresses\":[\"parent\"]}"
                    }
                }]
            }),
            json!({"role": "tool", "content": "sent"}),
            json!({"role": "assistant", "content": "Done"}),
        ];
        assert_eq!(
            terminal_result_without_explicit_parent_message(&explicit, "parent"),
            None
        );

        let sibling_only = vec![
            json!({"role": "user", "content": "Message sibling"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "function": {
                        "name": "send_message_to_agent",
                        "arguments": "{\"addresses\":[\"sibling\"]}"
                    }
                }]
            }),
            json!({"role": "tool", "content": "sent"}),
            json!({"role": "assistant", "content": "Sibling notified"}),
        ];
        assert_eq!(
            terminal_result_without_explicit_parent_message(&sibling_only, "parent"),
            Some("Sibling notified")
        );
    }

    #[test]
    fn custom_model_config_key_resolves_to_provider_slug_and_endpoint() {
        use warp_multi_agent_api::request::settings::custom_model_providers::{
            CustomEndpointSchema, CustomModel, CustomModelProvider,
        };

        let request = warp_multi_agent_api::Request {
            settings: Some(warp_multi_agent_api::request::Settings {
                model_config: Some(warp_multi_agent_api::request::settings::ModelConfig {
                    base: "custom-key".into(),
                    ..Default::default()
                }),
                custom_model_providers: Some(
                    warp_multi_agent_api::request::settings::CustomModelProviders {
                        providers: vec![CustomModelProvider {
                            base_url: "https://custom.example/v1".into(),
                            api_key: "test-key".into(),
                            models: vec![CustomModel {
                                slug: "provider-model".into(),
                                config_key: "custom-key".into(),
                                reasoning_effort: String::new(),
                            }],
                            schema: CustomEndpointSchema::OpenaiChatCompletions as i32,
                        }],
                    },
                ),
                ..Default::default()
            }),
            ..Default::default()
        };

        let (config, model) = resolve_backend(&request, "custom-key", &fallback_config()).unwrap();
        assert_eq!(model, "provider-model");
        assert_eq!(config.backend_base_url, "https://custom.example/v1");
        assert_eq!(config.backend_api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn drain_sse_line_handles_partial_buffers() {
        let mut buffer = b"data: one\r\n\r\npartial".to_vec();
        assert_eq!(drain_sse_line(&mut buffer).as_deref(), Some("data: one"));
        assert_eq!(drain_sse_line(&mut buffer).as_deref(), Some(""));
        assert!(drain_sse_line(&mut buffer).is_none());
        buffer.extend_from_slice(b" line\n");
        assert_eq!(drain_sse_line(&mut buffer).as_deref(), Some("partial line"));
    }

    #[tokio::test]
    async fn text_stream_is_committed_on_the_first_content_chunk() {
        let (text_tx, mut text_rx) = mpsc::channel(4);
        let (decision_tx, decision_rx) = oneshot::channel();
        let mut decision_tx = Some(decision_tx);
        let mut pending_text = Vec::new();
        let mut tool_calls = StreamToolCallAccumulator::default();
        let mut saw_tool_calls = false;
        let mut in_reasoning = false;

        let keep_reading = handle_backend_stream_line(
            r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#,
            &text_tx,
            &mut decision_tx,
            &mut pending_text,
            &mut tool_calls,
            &mut saw_tool_calls,
            &mut in_reasoning,
        )
        .await
        .unwrap();

        assert!(keep_reading);
        assert!(matches!(
            decision_rx.await.unwrap().unwrap(),
            BackendStreamDecision::Text
        ));
        assert_eq!(text_rx.recv().await.as_deref(), Some("Hello"));
    }

    #[tokio::test]
    async fn reasoning_waits_for_content_before_committing_text_stream() {
        let (text_tx, mut text_rx) = mpsc::channel(8);
        let (decision_tx, mut decision_rx) = oneshot::channel();
        let mut decision_tx = Some(decision_tx);
        let mut pending_text = Vec::new();
        let mut tool_calls = StreamToolCallAccumulator::default();
        let mut saw_tool_calls = false;
        let mut in_reasoning = false;

        handle_backend_stream_line(
            r#"data: {"choices":[{"delta":{"reasoning_content":"Thinking"}}]}"#,
            &text_tx,
            &mut decision_tx,
            &mut pending_text,
            &mut tool_calls,
            &mut saw_tool_calls,
            &mut in_reasoning,
        )
        .await
        .unwrap();
        assert!(matches!(
            decision_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(text_rx.try_recv().is_err());

        handle_backend_stream_line(
            r#"data: {"choices":[{"delta":{"content":"Answer"}}]}"#,
            &text_tx,
            &mut decision_tx,
            &mut pending_text,
            &mut tool_calls,
            &mut saw_tool_calls,
            &mut in_reasoning,
        )
        .await
        .unwrap();

        assert!(matches!(
            decision_rx.await.unwrap().unwrap(),
            BackendStreamDecision::Text
        ));
        assert_eq!(
            text_rx.recv().await.as_deref(),
            Some("<details><summary>💭 Thinking...</summary>\n\n")
        );
        assert_eq!(text_rx.recv().await.as_deref(), Some("Thinking"));
        assert_eq!(text_rx.recv().await.as_deref(), Some("\n\n</details>\n\n"));
        assert_eq!(text_rx.recv().await.as_deref(), Some("Answer"));
    }

    #[tokio::test]
    async fn backend_stream_returns_before_upstream_completion() {
        async fn delayed_chat_completion() -> Response<Body> {
            let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
            tokio::spawn(async move {
                tx.send(Ok(Bytes::from_static(
                    br#"data: {"choices":[{"delta":{"content":"STREAM_FIRST"}}]}

"#,
                )))
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                tx.send(Ok(Bytes::from_static(
                    br#"data: {"choices":[{"delta":{"content":"STREAM_SECOND"}}]}

data: [DONE]

"#,
                )))
                .await
                .unwrap();
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(ReceiverStream::new(rx)))
                .unwrap()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/chat/completions",
                    axum::routing::post(delayed_chat_completion),
                ),
            )
            .await
            .unwrap();
        });

        let mut config = fallback_config();
        config.backend_base_url = format!("http://{address}/v1");
        config.backend_auth_style = AuthStyle::None;
        let state = AppState::new(config.clone(), vec![]);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            call_backend_streaming(
                &state,
                &config,
                &[json!({"role": "user", "content": "stream"})],
                false,
                "mock-model",
                &json!([]),
            ),
        )
        .await
        .expect("first content chunk should commit text mode before upstream completion")
        .unwrap();

        let StreamingLlmResult::TextStream(mut text_rx) = result else {
            panic!("expected text stream");
        };
        assert_eq!(text_rx.recv().await.as_deref(), Some("STREAM_FIRST"));
    }

    #[test]
    fn stream_tool_calls_reassemble_from_deltas() {
        let mut accumulator = StreamToolCallAccumulator::default();
        accumulator.push_chunk(&[
            json!({
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": { "name": "run_shell_command", "arguments": "{\"command\":\"ec" }
            }),
            json!({
                "index": 1,
                "id": "call_2",
                "type": "function",
                "function": { "name": "grep", "arguments": "{\"queries\":[\"foo" }
            }),
        ]);
        accumulator.push_chunk(&[
            json!({
                "index": 0,
                "function": { "arguments": "ho hi\"}" }
            }),
            json!({
                "index": 1,
                "function": { "arguments": "\"]}" }
            }),
        ]);

        let tool_calls = accumulator.into_tool_calls();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0]["function"]["name"], "run_shell_command");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            "{\"command\":\"echo hi\"}"
        );
        assert_eq!(tool_calls[1]["function"]["name"], "grep");
        assert_eq!(
            tool_calls[1]["function"]["arguments"],
            "{\"queries\":[\"foo\"]}"
        );
    }
}
