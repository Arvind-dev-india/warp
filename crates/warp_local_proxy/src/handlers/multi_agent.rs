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

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use prost::Message;
use serde_json::json;

use crate::server::AppState;

// ── OpenAI tool definitions ──────────────────────────────────────────

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
                                            "options": { "type": "array", "items": { "type": "string" } },
                                            "is_multiselect": { "type": "boolean" },
                                            "supports_other": { "type": "boolean" },
                                            "recommended_option_index": { "type": "integer" }
                                        }
                                    }
                                },
                                "required": ["question_id", "question"]
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
                "description": "Suggest a follow-up prompt.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
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
                "description": "Perform computer use actions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action_summary": { "type": "string" }
                    },
                    "required": ["action_summary"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "request_computer_use",
                "description": "Request permission for computer use.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_summary": { "type": "string" }
                    },
                    "required": ["task_summary"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "subagent",
                "description": "Spawn a subagent.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "payload": { "type": "string" }
                    },
                    "required": ["task_id", "payload"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "start_agent",
                "description": "Start an agent.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "prompt": { "type": "string" }
                    },
                    "required": ["name", "prompt"]
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
                "name": "fetch_conversation",
                "description": "Fetch conversation data.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string" }
                    },
                    "required": ["conversation_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "upload_file_artifact",
                "description": "Upload a file artifact.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_agents",
                "description": "Run multiple agents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string" },
                        "base_prompt": { "type": "string" }
                    },
                    "required": ["summary", "base_prompt"]
                }
            }
        }
    ])
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
                            return q.user_query.as_ref().map(|uq| uq.query.clone())
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
        .map(|t| matches!(t, warp_multi_agent_api::request::input::Type::SummarizeConversation(_)))
        .unwrap_or(false)
}

fn extract_tool_results(request: &warp_multi_agent_api::Request) -> Vec<(String, String)> {
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
                    let text = request_tool_call_result_to_text(tcr);
                    results.push((tcr.tool_call_id.clone(), text));
                }
            }
        }
    }

    results
}

/// Convert a request-level ToolCallResult to text for the LLM.
fn request_tool_call_result_to_text(
    tcr: &warp_multi_agent_api::request::input::ToolCallResult,
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
                #[allow(deprecated)]
                format!(
                    "Command: {}\nExit code: {}\nOutput:\n{}",
                    r.command, r.exit_code, r.output
                )
            }
        },
        R::ReadFiles(r) => match &r.result {
            Some(warp_multi_agent_api::read_files_result::Result::TextFilesSuccess(s)) => s
                .files
                .iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>()
                .join("\n\n"),
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
        _ => "(tool result)".to_string(),
    }
}

// ── Message-level tool result → text (for history replay) ────────────

fn message_tool_call_result_to_text(tcr: &warp_multi_agent_api::message::ToolCallResult) -> String {
    let Some(ref result) = tcr.result else {
        return "(no result)".to_string();
    };

    use warp_multi_agent_api::message::tool_call_result::Result as R;
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
                #[allow(deprecated)]
                format!(
                    "Command: {}\nExit code: {}\nOutput:\n{}",
                    r.command, r.exit_code, r.output
                )
            }
        },
        R::ReadFiles(r) => match &r.result {
            Some(warp_multi_agent_api::read_files_result::Result::TextFilesSuccess(s)) => s
                .files
                .iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>()
                .join("\n\n"),
            Some(warp_multi_agent_api::read_files_result::Result::Error(e)) => {
                format!("Error reading files: {}", e.message)
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
                format!("Successfully applied diffs to: {}", files.join(", "))
            }
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Error(e)) => {
                format!("Diff error: {}", e.message)
            }
            None => "(no diff result)".to_string(),
        },
        R::SuggestPlan(_) => "Plan accepted by user.".to_string(),
        R::SuggestCreatePlan(r) => {
            if r.accepted {
                "Plan creation accepted.".into()
            } else {
                "Plan creation rejected.".into()
            }
        }
        R::Cancel(_) => "Tool call was cancelled by user.".to_string(),
        _ => "(tool result)".to_string(),
    }
}

// ── Build OpenAI messages from conversation history ──────────────────

fn build_openai_messages(
    request: &warp_multi_agent_api::Request,
    user_query: Option<&str>,
    tool_results: &[(String, String)],
) -> Vec<serde_json::Value> {
    let mut messages = vec![json!({
        "role": "system",
        "content": "You are a helpful AI assistant integrated into the Warp terminal. You have \
                    access to tools that let you run shell commands, read files, edit files, \
                    search code, and more. Use tools when needed to help the user. \
                    When providing code changes, use the apply_file_diffs tool. \
                    When you need to understand existing code, use read_files or grep. \
                    Be concise but thorough."
    })];

    // Replay conversation history from task_context.
    // First, collect all tool_call_ids that have matching results so we
    // can skip orphaned ToolCall messages (OpenAI requires every
    // assistant tool_call to be followed by a matching tool result).
    if let Some(ref tc) = request.task_context {
        let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut tool_call_count = 0u32;
        let mut tool_result_count = 0u32;
        let mut user_query_count = 0u32;
        let mut agent_output_count = 0u32;
        let mut other_count = 0u32;

        for task in &tc.tasks {
            for msg in &task.messages {
                match msg.message.as_ref() {
                    Some(warp_multi_agent_api::message::Message::ToolCallResult(tcr)) => {
                        result_ids.insert(tcr.tool_call_id.clone());
                        tool_result_count += 1;
                    }
                    Some(warp_multi_agent_api::message::Message::ToolCall(_)) => {
                        tool_call_count += 1;
                    }
                    Some(warp_multi_agent_api::message::Message::UserQuery(_)) => {
                        user_query_count += 1;
                    }
                    Some(warp_multi_agent_api::message::Message::AgentOutput(_)) => {
                        agent_output_count += 1;
                    }
                    _ => {
                        other_count += 1;
                    }
                }
            }
        }
        // Also count tool results from the current request input
        for (id, _) in tool_results {
            result_ids.insert(id.clone());
        }

        tracing::info!(
            tasks = tc.tasks.len(),
            tool_calls = tool_call_count,
            tool_results_in_history = tool_result_count,
            tool_results_in_input = tool_results.len(),
            matched_result_ids = result_ids.len(),
            user_queries = user_query_count,
            agent_outputs = agent_output_count,
            other = other_count,
            "conversation history summary"
        );

        for task in &tc.tasks {
            for msg in &task.messages {
                if let Some(ref m) = msg.message {
                    match m {
                        warp_multi_agent_api::message::Message::UserQuery(q) => {
                            messages.push(json!({ "role": "user", "content": q.query }));
                        }
                        warp_multi_agent_api::message::Message::AgentOutput(a)
                            if !a.text.is_empty() =>
                        {
                            messages.push(json!({ "role": "assistant", "content": a.text }));
                        }
                        warp_multi_agent_api::message::Message::ToolCall(tc) => {
                            let (fn_name, fn_args) = tool_call_to_openai(tc);
                            messages.push(json!({
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": tc.tool_call_id,
                                    "type": "function",
                                    "function": { "name": fn_name, "arguments": fn_args }
                                }]
                            }));
                            // If this tool call has no matching result in history
                            // or current input, synthesize a placeholder so
                            // OpenAI's API doesn't reject the orphaned tool_call.
                            if !result_ids.contains(&tc.tool_call_id) {
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tc.tool_call_id,
                                    "content": "(result not available)"
                                }));
                            }
                        }
                        warp_multi_agent_api::message::Message::ToolCallResult(tcr) => {
                            let text = message_tool_call_result_to_text(tcr);
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tcr.tool_call_id,
                                "content": text
                            }));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // New user query (fresh turn)
    if let Some(q) = user_query {
        if !q.is_empty() {
            messages.push(json!({ "role": "user", "content": q }));
        }
    }

    // Tool results from current request input (continuation turn)
    for (tool_call_id, result_text) in tool_results {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": result_text
        }));
    }

    messages
}

fn line_ranges_to_json(
    line_ranges: &[warp_multi_agent_api::FileContentLineRange],
) -> Vec<serde_json::Value> {
    line_ranges
        .iter()
        .map(|range| json!({ "start": range.start, "end": range.end }))
        .collect()
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

fn write_mode_to_json(
    mode: Option<
        &warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::Mode,
    >,
) -> &'static str {
    use warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::mode::Mode;

    match mode.and_then(|mode| mode.mode.as_ref()) {
        Some(Mode::Line(_)) => "line",
        Some(Mode::Block(_)) => "block",
        _ => "raw",
    }
}

fn json_to_write_mode(
    mode: Option<&str>,
) -> Option<warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::Mode> {
    use warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::{
        mode::Mode, Mode as WriteMode,
    };

    mode.map(|mode| WriteMode {
        mode: Some(match mode {
            "line" => Mode::Line(()),
            "block" => Mode::Block(()),
            _ => Mode::Raw(()),
        }),
    })
}

fn prost_value_to_json(value: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;

    match value.kind.as_ref() {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(v)) => serde_json::Value::Bool(*v),
        Some(Kind::NumberValue(v)) => serde_json::Number::from_f64(*v)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(Kind::StringValue(v)) => serde_json::Value::String(v.clone()),
        Some(Kind::StructValue(v)) => prost_struct_to_json(v),
        Some(Kind::ListValue(v)) => {
            serde_json::Value::Array(v.values.iter().map(prost_value_to_json).collect())
        }
        None => serde_json::Value::Null,
    }
}

fn prost_struct_to_json(value: &prost_types::Struct) -> serde_json::Value {
    serde_json::Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), prost_value_to_json(value)))
            .collect(),
    )
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

fn tool_call_to_openai(tc: &warp_multi_agent_api::message::ToolCall) -> (String, String) {
    let Some(ref tool) = tc.tool else {
        return ("unknown".into(), "{}".into());
    };
    use warp_multi_agent_api::message::tool_call::Tool;
    match tool {
        Tool::RunShellCommand(cmd) => (
            "run_shell_command".into(),
            json!({ "command": cmd.command, "is_read_only": cmd.is_read_only }).to_string(),
        ),
        Tool::WriteToLongRunningShellCommand(cmd) => (
            "write_to_long_running_shell_command".into(),
            json!({
                "input": String::from_utf8_lossy(&cmd.input),
                "mode": write_mode_to_json(cmd.mode.as_ref()),
                "command_id": cmd.command_id,
            })
            .to_string(),
        ),
        Tool::ReadShellCommandOutput(cmd) => {
            let mut payload = json!({ "command_id": cmd.command_id });
            match cmd.delay.as_ref() {
                Some(warp_multi_agent_api::message::tool_call::read_shell_command_output::Delay::Duration(duration)) => {
                    payload["delay_seconds"] = json!(duration.seconds);
                }
                Some(warp_multi_agent_api::message::tool_call::read_shell_command_output::Delay::OnCompletion(_)) => {
                    payload["wait_for_completion"] = json!(true);
                }
                None => {}
            }
            ("read_shell_command_output".into(), payload.to_string())
        }
        Tool::TransferShellCommandControlToUser(cmd) => (
            "transfer_shell_command_control_to_user".into(),
            json!({ "reason": cmd.reason }).to_string(),
        ),
        Tool::ReadFiles(rf) => {
            let files: Vec<_> = rf
                .files
                .iter()
                .map(|file| json!({ "name": file.name, "line_ranges": line_ranges_to_json(&file.line_ranges) }))
                .collect();
            ("read_files".into(), json!({ "files": files }).to_string())
        }
        Tool::ReadDocuments(rd) => {
            let documents: Vec<_> = rd
                .documents
                .iter()
                .map(|document| {
                    json!({
                        "document_id": document.document_id,
                        "line_ranges": line_ranges_to_json(&document.line_ranges),
                    })
                })
                .collect();
            ("read_documents".into(), json!({ "documents": documents }).to_string())
        }
        Tool::ApplyFileDiffs(afd) => {
            let diffs: Vec<_> = afd
                .diffs
                .iter()
                .map(|diff| json!({ "file_path": diff.file_path, "search": diff.search, "replace": diff.replace }))
                .collect();
            let new_files: Vec<_> = afd
                .new_files
                .iter()
                .map(|file| json!({ "file_path": file.file_path, "content": file.content }))
                .collect();
            let deleted_files: Vec<_> = afd
                .deleted_files
                .iter()
                .map(|file| json!({ "file_path": file.file_path }))
                .collect();
            let v4a_updates: Vec<_> = afd
                .v4a_updates
                .iter()
                .map(|update| {
                    json!({
                        "file_path": update.file_path,
                        "move_to": update.move_to,
                        "hunks": update
                            .hunks
                            .iter()
                            .map(|hunk| {
                                json!({
                                    "change_context": hunk.change_context,
                                    "pre_context": hunk.pre_context,
                                    "old": hunk.old,
                                    "new": hunk.new,
                                    "post_context": hunk.post_context,
                                })
                            })
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            (
                "apply_file_diffs".into(),
                json!({
                    "summary": afd.summary,
                    "diffs": diffs,
                    "new_files": new_files,
                    "deleted_files": deleted_files,
                    "v4a_updates": v4a_updates,
                })
                .to_string(),
            )
        }
        Tool::EditDocuments(ed) => {
            let diffs: Vec<_> = ed
                .diffs
                .iter()
                .map(|diff| json!({ "document_id": diff.document_id, "search": diff.search, "replace": diff.replace }))
                .collect();
            ("edit_documents".into(), json!({ "diffs": diffs }).to_string())
        }
        Tool::CreateDocuments(cd) => {
            let new_documents: Vec<_> = cd
                .new_documents
                .iter()
                .map(|document| json!({ "content": document.content, "title": document.title }))
                .collect();
            ("create_documents".into(), json!({ "new_documents": new_documents }).to_string())
        }
        Tool::Grep(g) => (
            "grep".into(),
            json!({ "queries": g.queries, "path": g.path }).to_string(),
        ),
        Tool::FileGlobV2(fg) => (
            "file_glob".into(),
            json!({
                "patterns": fg.patterns,
                "search_dir": fg.search_dir,
                "max_matches": fg.max_matches,
                "max_depth": fg.max_depth,
                "min_depth": fg.min_depth,
            })
            .to_string(),
        ),
        Tool::SearchCodebase(sc) => (
            "search_codebase".into(),
            json!({
                "query": sc.query,
                "path_filters": sc.path_filters,
                "codebase_path": sc.codebase_path,
            })
            .to_string(),
        ),
        Tool::SuggestPlan(plan) => (
            "suggest_plan".into(),
            json!({ "summary": plan.summary }).to_string(),
        ),
        Tool::SuggestCreatePlan(_) => ("suggest_create_plan".into(), "{}".into()),
        Tool::ReadMcpResource(resource) => (
            "read_mcp_resource".into(),
            json!({ "uri": resource.uri, "server_id": resource.server_id }).to_string(),
        ),
        Tool::CallMcpTool(tool_call) => (
            "call_mcp_tool".into(),
            json!({
                "name": tool_call.name,
                "args": tool_call.args.as_ref().map(prost_struct_to_json).unwrap_or(serde_json::Value::Null),
                "server_id": tool_call.server_id,
            })
            .to_string(),
        ),
        Tool::SuggestPrompt(prompt) => {
            let text = match prompt.display_mode.as_ref() {
                Some(warp_multi_agent_api::message::tool_call::suggest_prompt::DisplayMode::PromptChip(chip)) => {
                    chip.prompt.clone()
                }
                Some(warp_multi_agent_api::message::tool_call::suggest_prompt::DisplayMode::InlineQueryBanner(banner)) => {
                    banner.query.clone()
                }
                None => String::new(),
            };
            ("suggest_prompt".into(), json!({ "text": text }).to_string())
        }
        Tool::OpenCodeReview(_) => ("open_code_review".into(), "{}".into()),
        Tool::InitProject(_) => ("init_project".into(), "{}".into()),
        Tool::Subagent(subagent) => (
            "subagent".into(),
            json!({ "task_id": subagent.task_id, "payload": subagent.payload }).to_string(),
        ),
        Tool::UseComputer(use_computer) => (
            "use_computer".into(),
            json!({ "action_summary": use_computer.action_summary }).to_string(),
        ),
        Tool::InsertReviewComments(review) => {
            let comments: Vec<_> = review
                .comments
                .iter()
                .map(|comment| {
                    let mut value = json!({
                        "comment_id": comment.comment_id,
                        "author": comment.author,
                        "last_modified_timestamp": comment.last_modified_timestamp,
                        "comment_body": comment.comment_body,
                        "parent_comment_id": comment.parent_comment_id,
                        "html_url": comment.html_url,
                    });
                    if let Some(location) = comment.location.as_ref() {
                        let mut location_value = json!({ "file_path": location.file_path });
                        if let Some(line) = location.line.as_ref() {
                            let side = match warp_multi_agent_api::message::tool_call::insert_review_comments::CommentSide::try_from(line.side)
                            {
                                Ok(warp_multi_agent_api::message::tool_call::insert_review_comments::CommentSide::Old) => "old",
                                _ => "new",
                            };
                            location_value["line"] = json!({
                                "diff_hunk": line.diff_hunk,
                                "range": line.range.as_ref().map(|range| json!({ "start": range.start, "end": range.end })),
                                "side": side,
                            });
                        }
                        value["location"] = location_value;
                    }
                    value
                })
                .collect();
            (
                "insert_review_comments".into(),
                json!({
                    "repo_path": review.repo_path,
                    "comments": comments,
                    "base_branch": review.base_branch,
                })
                .to_string(),
            )
        }
        Tool::ReadSkill(skill) => {
            let mut payload = json!({ "name": skill.name });
            match skill.skill_reference.as_ref() {
                Some(warp_multi_agent_api::message::tool_call::read_skill::SkillReference::SkillPath(path)) => {
                    payload["skill_path"] = json!(path);
                }
                Some(warp_multi_agent_api::message::tool_call::read_skill::SkillReference::BundledSkillId(id)) => {
                    payload["bundled_skill_id"] = json!(id);
                }
                None => {}
            }
            ("read_skill".into(), payload.to_string())
        }
        Tool::RequestComputerUse(request) => (
            "request_computer_use".into(),
            json!({ "task_summary": request.task_summary }).to_string(),
        ),
        Tool::FetchConversation(fetch) => (
            "fetch_conversation".into(),
            json!({ "conversation_id": fetch.conversation_id }).to_string(),
        ),
        Tool::StartAgent(agent) => (
            "start_agent".into(),
            json!({ "name": agent.name, "prompt": agent.prompt }).to_string(),
        ),
        Tool::SendMessageToAgent(message) => (
            "send_message_to_agent".into(),
            json!({
                "addresses": message.addresses,
                "subject": message.subject,
                "message": message.message,
            })
            .to_string(),
        ),
        Tool::AskUserQuestion(question) => {
            let questions: Vec<_> = question
                .questions
                .iter()
                .map(|question| {
                    let mut value = json!({
                        "question_id": question.question_id,
                        "question": question.question,
                    });
                    if let Some(warp_multi_agent_api::ask_user_question::question::QuestionType::MultipleChoice(multiple_choice)) =
                        question.question_type.as_ref()
                    {
                        value["multiple_choice"] = json!({
                            "options": multiple_choice
                                .options
                                .iter()
                                .map(|option| option.label.clone())
                                .collect::<Vec<_>>(),
                            "is_multiselect": multiple_choice.is_multiselect,
                            "supports_other": multiple_choice.supports_other,
                            "recommended_option_index": multiple_choice.recommended_option_index,
                        });
                    }
                    value
                })
                .collect();
            ("ask_user_question".into(), json!({ "questions": questions }).to_string())
        }
        Tool::UploadFileArtifact(artifact) => (
            "upload_file_artifact".into(),
            json!({ "description": artifact.description }).to_string(),
        ),
        Tool::RunAgents(run_agents) => (
            "run_agents".into(),
            json!({ "summary": run_agents.summary, "base_prompt": run_agents.base_prompt }).to_string(),
        ),
        _ => ("unknown".into(), "{}".into()),
    }
}

// ── OpenAI tool_call → protobuf ToolCall ─────────────────────────────

fn openai_tool_call_to_proto(
    tc: &serde_json::Value,
) -> Option<warp_multi_agent_api::message::ToolCall> {
    let fn_name = tc["function"]["name"].as_str()?;
    let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(json!({}));
    let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();

    use warp_multi_agent_api::message::tool_call::Tool;
    let tool = match fn_name {
        "run_shell_command" => Tool::RunShellCommand(warp_multi_agent_api::message::tool_call::RunShellCommand {
            command: args["command"].as_str().unwrap_or("").into(),
            #[allow(deprecated)]
            is_read_only: args["is_read_only"].as_bool().unwrap_or(true),
            ..Default::default()
        }),
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
            } else if let Some(delay_seconds) = args["delay_seconds"].as_i64() {
                Some(warp_multi_agent_api::message::tool_call::read_shell_command_output::Delay::Duration(
                    prost_types::Duration {
                        seconds: delay_seconds,
                        nanos: 0,
                    },
                ))
            } else {
                None
            };
            Tool::ReadShellCommandOutput(warp_multi_agent_api::message::tool_call::ReadShellCommandOutput {
                command_id: args["command_id"].as_str().unwrap_or("").into(),
                delay,
            })
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
            Tool::ReadDocuments(warp_multi_agent_api::message::tool_call::ReadDocuments { documents })
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
            Tool::CreateDocuments(warp_multi_agent_api::message::tool_call::CreateDocuments { new_documents })
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
        "search_codebase" => Tool::SearchCodebase(warp_multi_agent_api::message::tool_call::SearchCodebase {
            query: args["query"].as_str().unwrap_or("").into(),
            path_filters: json_string_array(&args["path_filters"]),
            codebase_path: args["codebase_path"].as_str().unwrap_or("").into(),
        }),
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
            Tool::InsertReviewComments(warp_multi_agent_api::message::tool_call::InsertReviewComments {
                repo_path: args["repo_path"].as_str().unwrap_or("").into(),
                comments,
                base_branch: args["base_branch"].as_str().unwrap_or("").into(),
            })
        }
        "open_code_review" => Tool::OpenCodeReview(warp_multi_agent_api::message::tool_call::OpenCodeReview {}),
        "suggest_plan" => Tool::SuggestPlan(warp_multi_agent_api::message::tool_call::SuggestPlan {
            summary: args["summary"].as_str().unwrap_or("").into(),
            proposed_tasks: vec![],
        }),
        "suggest_create_plan" => Tool::SuggestCreatePlan(warp_multi_agent_api::message::tool_call::SuggestCreatePlan {}),
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
        "suggest_prompt" => Tool::SuggestPrompt(warp_multi_agent_api::message::tool_call::SuggestPrompt {
            display_mode: Some(
                warp_multi_agent_api::message::tool_call::suggest_prompt::DisplayMode::PromptChip(
                    warp_multi_agent_api::message::tool_call::suggest_prompt::PromptChip {
                        prompt: args["text"].as_str().unwrap_or("").into(),
                        label: String::new(),
                    },
                ),
            ),
            ..Default::default()
        }),
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
        "read_mcp_resource" => Tool::ReadMcpResource(warp_multi_agent_api::message::tool_call::ReadMcpResource {
            uri: args["uri"].as_str().unwrap_or("").into(),
            server_id: args["server_id"].as_str().unwrap_or("").into(),
        }),
        "init_project" => Tool::InitProject(warp_multi_agent_api::message::tool_call::InitProject {}),
        "use_computer" => Tool::UseComputer(warp_multi_agent_api::message::tool_call::UseComputer {
            action_summary: args["action_summary"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        "request_computer_use" => Tool::RequestComputerUse(
            warp_multi_agent_api::message::tool_call::RequestComputerUse {
                task_summary: args["task_summary"].as_str().unwrap_or("").into(),
                ..Default::default()
            },
        ),
        "subagent" => Tool::Subagent(warp_multi_agent_api::message::tool_call::Subagent {
            task_id: args["task_id"].as_str().unwrap_or("").into(),
            payload: args["payload"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        "start_agent" => Tool::StartAgent(warp_multi_agent_api::StartAgent {
            name: args["name"].as_str().unwrap_or("").into(),
            prompt: args["prompt"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        "send_message_to_agent" => Tool::SendMessageToAgent(warp_multi_agent_api::SendMessageToAgent {
            addresses: json_string_array(&args["addresses"]),
            subject: args["subject"].as_str().unwrap_or("").into(),
            message: args["message"].as_str().unwrap_or("").into(),
        }),
        "fetch_conversation" => Tool::FetchConversation(warp_multi_agent_api::message::tool_call::FetchConversation {
            conversation_id: args["conversation_id"].as_str().unwrap_or("").into(),
        }),
        "upload_file_artifact" => Tool::UploadFileArtifact(warp_multi_agent_api::UploadFileArtifact {
            description: args["description"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        "run_agents" => Tool::RunAgents(warp_multi_agent_api::RunAgents {
            summary: args["summary"].as_str().unwrap_or("").into(),
            base_prompt: args["base_prompt"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        _ => return None,
    };

    Some(warp_multi_agent_api::message::ToolCall {
        tool_call_id,
        tool: Some(tool),
    })
}

// ── Main handler ─────────────────────────────────────────────────────

pub async fn handle(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
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

    let user_query = extract_user_query(&request);
    let tool_results = extract_tool_results(&request);

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
        is_continuation,
        existing_task = existing_task_id.as_deref().unwrap_or("(none)"),
        "multi-agent request"
    );

    // Reuse existing IDs when continuing a conversation, generate new ones otherwise
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    let task_id = existing_task_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

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
        openai_messages.push(json!({
            "role": "system",
            "content": "You are a helpful AI assistant integrated into the Warp terminal. You have \
                        access to tools that let you run shell commands, read files, edit files, \
                        search code, and more. Use tools when needed to help the user. \
                        When providing code changes, use the apply_file_diffs tool. \
                        When you need to understand existing code, use read_files or grep. \
                        Be concise but thorough."
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
        let summary_messages: Vec<serde_json::Value> = serde_json::from_value(summary_prompt).unwrap_or_default();
        let summary_result = call_backend_with_tools(&state, &summary_messages, false).await;
        if let Ok(LlmResult { response: LlmResponse::Text(ref summary), .. }) = summary_result {
            // Replace conversation with system + summary
            openai_messages = vec![
                openai_messages[0].clone(), // keep system prompt
                json!({ "role": "assistant", "content": format!("[Conversation summary]\n{summary}") }),
            ];
            state.save_conversation(&task_id, &openai_messages);

            // Emit a Summarization message so the UI shows it
            let msg_id = uuid::Uuid::new_v4().to_string();
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
            emit_agent_output(&mut sse_body, &task_id, &request_id, &format!("Conversation compacted. Summary:\n{summary}"));
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

    // Count tool call rounds for the max-rounds limit
    const MAX_TOOL_ROUNDS: u32 = 15;
    let prior_tool_rounds = openai_messages
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

    let llm_result = call_backend_with_tools(&state, &openai_messages, send_tools).await;
    let context_usage = llm_result.as_ref().map(|r| r.context_usage).unwrap_or(0.0);

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

    // Handle LLM response
    match llm_result {
        Ok(LlmResult { response: LlmResponse::Text(ref text), .. }) => {
            // Save assistant text to cache
            openai_messages.push(json!({ "role": "assistant", "content": text }));
            emit_agent_output(&mut sse_body, &task_id, &request_id, text);
        }
        Ok(LlmResult { response: LlmResponse::ToolCalls(ref tool_calls), .. }) => {
            // Save the assistant tool_calls message to cache (one message per call
            // to match OpenAI's expectation)
            for tc in tool_calls {
                let fn_name = tc["function"]["name"].as_str().unwrap_or("unknown");
                let fn_args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let tc_id = tc["id"].as_str().unwrap_or("");
                openai_messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": tc_id,
                        "type": "function",
                        "function": { "name": fn_name, "arguments": fn_args }
                    }]
                }));

                if let Some(proto_tc) = openai_tool_call_to_proto(tc) {
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
        }
        Err(e) => {
            tracing::error!("Backend call failed: {e}");
            openai_messages.push(json!({ "role": "assistant", "content": format!("Error: {e}") }));
            emit_agent_output(&mut sse_body, &task_id, &request_id, &format!("Error: {e}"));
        }
    }

    // Persist conversation to disk
    state.save_conversation(&task_id, &openai_messages);

    // StreamFinished
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
                        context_window_usage: context_usage,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        )),
    }));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(sse_body))
        .unwrap()
}

// ── Emit agent text output ───────────────────────────────────────────

fn emit_agent_output(sse_body: &mut String, task_id: &str, request_id: &str, text: &str) {
    let msg_id = uuid::Uuid::new_v4().to_string();

    // Add empty message placeholder
    sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.into(),
                                messages: vec![warp_multi_agent_api::Message {
                                    id: msg_id.clone(),
                                    task_id: task_id.into(),
                                    request_id: request_id.into(),
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
    }));

    // Append text content
    sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AppendToMessageContent(
                            warp_multi_agent_api::client_action::AppendToMessageContent {
                                task_id: task_id.into(),
                                message: Some(warp_multi_agent_api::Message {
                                    id: msg_id,
                                    task_id: task_id.into(),
                                    request_id: request_id.into(),
                                    message: Some(
                                        warp_multi_agent_api::message::Message::AgentOutput(
                                            warp_multi_agent_api::message::AgentOutput {
                                                text: text.into(),
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
    }));
}

// ── LLM backend ──────────────────────────────────────────────────────

enum LlmResponse {
    Text(String),
    ToolCalls(Vec<serde_json::Value>),
}

struct LlmResult {
    response: LlmResponse,
    /// Fraction of context window used (0.0–1.0), from usage data.
    context_usage: f32,
}

async fn call_backend_with_tools(
    state: &AppState,
    messages: &[serde_json::Value],
    send_tools: bool,
) -> Result<LlmResult, anyhow::Error> {
    let url = state.config.chat_completions_url();
    let model = &state.config.default_model;

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": 4096,
        "stream": false
    });

    if send_tools {
        payload["tools"] = openai_tools();
    }

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

    let response_json: serde_json::Value = resp.json().await?;
    let choice = &response_json["choices"][0];
    let message = &choice["message"];

    // Estimate context window usage from token counts
    let prompt_tokens = response_json["usage"]["prompt_tokens"].as_f64().unwrap_or(0.0);
    let total_tokens = response_json["usage"]["total_tokens"].as_f64().unwrap_or(0.0);
    // Common context windows: 128k for most models, use prompt_tokens/128000 as estimate
    let context_limit = 128_000.0_f64;
    let context_usage = (total_tokens / context_limit).min(1.0) as f32;

    if let Some(tool_calls) = message["tool_calls"].as_array() {
        if !tool_calls.is_empty() {
            tracing::info!(count = tool_calls.len(), prompt_tokens, "LLM requested tool calls");
            return Ok(LlmResult {
                response: LlmResponse::ToolCalls(tool_calls.clone()),
                context_usage,
            });
        }
    }

    let text = message["content"]
        .as_str()
        .unwrap_or("(no response)")
        .to_string();
    Ok(LlmResult {
        response: LlmResponse::Text(text),
        context_usage,
    })
}
