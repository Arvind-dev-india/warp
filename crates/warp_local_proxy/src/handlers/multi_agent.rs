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
                "description": "Run a shell command in the user's terminal. Use for installing packages, running tests, building code, git operations, etc.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute" },
                        "is_read_only": { "type": "boolean", "description": "Whether the command only reads data (true) or modifies state (false)" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_files",
                "description": "Read the contents of one or more files. Use to understand code, check configs, etc.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string", "description": "File path relative to the working directory" },
                                    "line_ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "start": { "type": "integer" },
                                                "end": { "type": "integer" }
                                            }
                                        },
                                        "description": "Optional line ranges to read. If empty, reads the entire file."
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
                "name": "apply_file_diffs",
                "description": "Apply diffs/edits to files. Use for creating new files or modifying existing ones. Provide unified diff format for edits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string", "description": "Path to the file" },
                                    "diff": { "type": "string", "description": "The unified diff to apply, or full content for new files" }
                                },
                                "required": ["file_path", "diff"]
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
                "name": "grep",
                "description": "Search for a pattern in files using regex. Returns matching file paths and line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern to search for" },
                        "path": { "type": "string", "description": "Directory or file to search in (default: current directory)" },
                        "include_pattern": { "type": "string", "description": "Glob to filter files (e.g. '*.rs')" }
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "file_glob",
                "description": "Find files matching a glob pattern. Use to discover project structure.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern (e.g. 'src/**/*.rs')" }
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_codebase",
                "description": "Semantic search across the codebase. Use when you need to find relevant code by concept.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Natural language description of what to search for" }
                    },
                    "required": ["query"]
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

fn extract_tool_results(request: &warp_multi_agent_api::Request) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let Some(input) = request.input.as_ref() else { return results };
    let Some(input_type) = input.r#type.as_ref() else { return results };

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
                format!("Command: {}\nExit code: {}\nOutput:\n{}", r.command, f.exit_code, f.output)
            }
            Some(warp_multi_agent_api::run_shell_command_result::Result::LongRunningCommandSnapshot(s)) => {
                format!("Command: {} (still running)\nOutput so far:\n{}", r.command, s.output)
            }
            Some(warp_multi_agent_api::run_shell_command_result::Result::PermissionDenied(_)) => {
                format!("Command: {} — Permission denied by user", r.command)
            }
            None => {
                #[allow(deprecated)]
                format!("Command: {}\nExit code: {}\nOutput:\n{}", r.command, r.exit_code, r.output)
            }
        },
        R::ReadFiles(r) => match &r.result {
            Some(warp_multi_agent_api::read_files_result::Result::TextFilesSuccess(s)) => s
                .files.iter().map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>().join("\n\n"),
            Some(warp_multi_agent_api::read_files_result::Result::Error(e)) => format!("Error: {}", e.message),
            _ => "(file read result)".to_string(),
        },
        R::Grep(r) => match &r.result {
            Some(warp_multi_agent_api::grep_result::Result::Success(s)) => s
                .matched_files.iter().map(|f| {
                    let lines: Vec<String> = f.matched_lines.iter().map(|l| l.line_number.to_string()).collect();
                    format!("{}:{}", f.file_path, lines.join(","))
                }).collect::<Vec<_>>().join("\n"),
            Some(warp_multi_agent_api::grep_result::Result::Error(e)) => format!("Grep error: {}", e.message),
            None => "(no grep result)".to_string(),
        },
        R::FileGlobV2(r) => match &r.result {
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Success(s)) => s
                .matched_files.iter().map(|f| f.file_path.as_str()).collect::<Vec<_>>().join("\n"),
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Error(e)) => format!("Glob error: {}", e.message),
            None => "(no glob result)".to_string(),
        },
        R::SearchCodebase(r) => match &r.result {
            Some(warp_multi_agent_api::search_codebase_result::Result::Success(s)) => s
                .files.iter().map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>().join("\n\n"),
            Some(warp_multi_agent_api::search_codebase_result::Result::Error(e)) => format!("Search error: {}", e.message),
            None => "(no search result)".to_string(),
        },
        R::ApplyFileDiffs(r) => match &r.result {
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Success(s)) => {
                let files: Vec<_> = s.updated_files_v2.iter()
                    .filter_map(|f| f.file.as_ref()).map(|f| f.file_path.as_str()).collect();
                format!("Applied diffs to: {}", files.join(", "))
            }
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Error(e)) => format!("Diff error: {}", e.message),
            None => "(no diff result)".to_string(),
        },
        R::SuggestPlan(_) => "Plan accepted.".to_string(),
        R::SuggestCreatePlan(r) => if r.accepted { "Plan accepted.".into() } else { "Plan rejected.".into() },
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
                format!("Command: {}\nExit code: {}\nOutput:\n{}", r.command, f.exit_code, f.output)
            }
            Some(warp_multi_agent_api::run_shell_command_result::Result::LongRunningCommandSnapshot(s)) => {
                format!("Command: {} (still running)\nOutput so far:\n{}", r.command, s.output)
            }
            Some(warp_multi_agent_api::run_shell_command_result::Result::PermissionDenied(_)) => {
                format!("Command: {} — Permission denied by user", r.command)
            }
            None => {
                #[allow(deprecated)]
                format!("Command: {}\nExit code: {}\nOutput:\n{}", r.command, r.exit_code, r.output)
            }
        },
        R::ReadFiles(r) => match &r.result {
            Some(warp_multi_agent_api::read_files_result::Result::TextFilesSuccess(s)) => s
                .files.iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>().join("\n\n"),
            Some(warp_multi_agent_api::read_files_result::Result::Error(e)) => {
                format!("Error reading files: {}", e.message)
            }
            _ => "(file read result)".to_string(),
        },
        R::Grep(r) => match &r.result {
            Some(warp_multi_agent_api::grep_result::Result::Success(s)) => s
                .matched_files.iter()
                .map(|f| {
                    let lines: Vec<String> = f.matched_lines.iter().map(|l| l.line_number.to_string()).collect();
                    format!("{}:{}", f.file_path, lines.join(","))
                })
                .collect::<Vec<_>>().join("\n"),
            Some(warp_multi_agent_api::grep_result::Result::Error(e)) => format!("Grep error: {}", e.message),
            None => "(no grep result)".to_string(),
        },
        R::FileGlobV2(r) => match &r.result {
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Success(s)) => s
                .matched_files.iter().map(|f| f.file_path.as_str()).collect::<Vec<_>>().join("\n"),
            Some(warp_multi_agent_api::file_glob_v2_result::Result::Error(e)) => format!("Glob error: {}", e.message),
            None => "(no glob result)".to_string(),
        },
        R::SearchCodebase(r) => match &r.result {
            Some(warp_multi_agent_api::search_codebase_result::Result::Success(s)) => s
                .files.iter()
                .map(|f| format!("=== {} ===\n{}", f.file_path, f.content))
                .collect::<Vec<_>>().join("\n\n"),
            Some(warp_multi_agent_api::search_codebase_result::Result::Error(e)) => format!("Search error: {}", e.message),
            None => "(no search result)".to_string(),
        },
        R::ApplyFileDiffs(r) => match &r.result {
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Success(s)) => {
                let files: Vec<_> = s.updated_files_v2.iter()
                    .filter_map(|f| f.file.as_ref()).map(|f| f.file_path.as_str()).collect();
                format!("Successfully applied diffs to: {}", files.join(", "))
            }
            Some(warp_multi_agent_api::apply_file_diffs_result::Result::Error(e)) => format!("Diff error: {}", e.message),
            None => "(no diff result)".to_string(),
        },
        R::SuggestPlan(_) => "Plan accepted by user.".to_string(),
        R::SuggestCreatePlan(r) => if r.accepted { "Plan creation accepted.".into() } else { "Plan creation rejected.".into() },
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

    // Replay conversation history from task_context
    if let Some(ref tc) = request.task_context {
        for task in &tc.tasks {
            for msg in &task.messages {
                if let Some(ref m) = msg.message {
                    match m {
                        warp_multi_agent_api::message::Message::UserQuery(q) => {
                            messages.push(json!({ "role": "user", "content": q.query }));
                        }
                        warp_multi_agent_api::message::Message::AgentOutput(a) if !a.text.is_empty() => {
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

fn tool_call_to_openai(tc: &warp_multi_agent_api::message::ToolCall) -> (String, String) {
    let Some(ref tool) = tc.tool else {
        return ("unknown".into(), "{}".into());
    };
    use warp_multi_agent_api::message::tool_call::Tool;
    match tool {
        Tool::RunShellCommand(cmd) => ("run_shell_command".into(),
            json!({ "command": cmd.command, "is_read_only": cmd.is_read_only }).to_string()),
        Tool::ReadFiles(rf) => {
            let files: Vec<_> = rf.files.iter().map(|f| json!({ "name": f.name })).collect();
            ("read_files".into(), json!({ "files": files }).to_string())
        }
        Tool::ApplyFileDiffs(afd) => {
            let files: Vec<_> = afd.new_files.iter()
                .map(|f| json!({ "file_path": f.file_path, "diff": f.content })).collect();
            ("apply_file_diffs".into(), json!({ "files": files }).to_string())
        }
        Tool::Grep(g) => ("grep".into(),
            json!({ "queries": g.queries, "path": g.path }).to_string()),
        Tool::FileGlobV2(fg) => ("file_glob".into(), json!({ "patterns": fg.patterns }).to_string()),
        Tool::SearchCodebase(sc) => ("search_codebase".into(), json!({ "query": sc.query }).to_string()),
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
        "run_shell_command" => {
            Tool::RunShellCommand(warp_multi_agent_api::message::tool_call::RunShellCommand {
                command: args["command"].as_str().unwrap_or("").into(),
                #[allow(deprecated)]
                is_read_only: args["is_read_only"].as_bool().unwrap_or(true),
                ..Default::default()
            })
        }
        "read_files" => {
            let files = args["files"].as_array().map(|arr| arr.iter().filter_map(|f| {
                Some(warp_multi_agent_api::message::tool_call::read_files::File {
                    name: f["name"].as_str()?.into(),
                    line_ranges: vec![],
                })
            }).collect()).unwrap_or_default();
            Tool::ReadFiles(warp_multi_agent_api::message::tool_call::ReadFiles { files })
        }
        "apply_file_diffs" => {
            let new_files = args["files"].as_array().map(|arr| arr.iter().filter_map(|f| {
                Some(warp_multi_agent_api::message::tool_call::apply_file_diffs::NewFile {
                    file_path: f["file_path"].as_str()?.into(),
                    content: f["diff"].as_str().unwrap_or("").into(),
                })
            }).collect()).unwrap_or_default();
            Tool::ApplyFileDiffs(warp_multi_agent_api::message::tool_call::ApplyFileDiffs {
                new_files,
                ..Default::default()
            })
        }
        "grep" => Tool::Grep(warp_multi_agent_api::message::tool_call::Grep {
            queries: vec![args["pattern"].as_str().unwrap_or("").into()],
            path: args["path"].as_str().unwrap_or("").into(),
        }),
        "file_glob" => Tool::FileGlobV2(warp_multi_agent_api::message::tool_call::FileGlobV2 {
            patterns: vec![args["pattern"].as_str().unwrap_or("").into()],
            ..Default::default()
        }),
        "search_codebase" => Tool::SearchCodebase(warp_multi_agent_api::message::tool_call::SearchCodebase {
            query: args["query"].as_str().unwrap_or("").into(),
            ..Default::default()
        }),
        _ => return None,
    };

    Some(warp_multi_agent_api::message::ToolCall { tool_call_id, tool: Some(tool) })
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
    let is_continuation = !tool_results.is_empty();

    tracing::info!(
        query = user_query.as_deref().unwrap_or("(none)"),
        tool_results = tool_results.len(),
        is_continuation,
        "multi-agent request"
    );

    let conversation_id = uuid::Uuid::new_v4().to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    let task_id = uuid::Uuid::new_v4().to_string();

    let openai_messages = build_openai_messages(&request, user_query.as_deref(), &tool_results);
    let llm_response = call_backend_with_tools(&state, &openai_messages).await;

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
                                            message: Some(warp_multi_agent_api::message::Message::UserQuery(
                                                warp_multi_agent_api::message::UserQuery {
                                                    query: q.clone(),
                                                    ..Default::default()
                                                },
                                            )),
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
    match llm_response {
        Ok(LlmResponse::Text(text)) => {
            emit_agent_output(&mut sse_body, &task_id, &request_id, &text);
        }
        Ok(LlmResponse::ToolCalls(tool_calls)) => {
            for tc in &tool_calls {
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
            emit_agent_output(&mut sse_body, &task_id, &request_id, &format!("Error: {e}"));
        }
    }

    // StreamFinished
    sse_body.push_str(&sse_line(&warp_multi_agent_api::ResponseEvent {
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
                                    message: Some(warp_multi_agent_api::message::Message::AgentOutput(
                                        warp_multi_agent_api::message::AgentOutput { text: String::new() },
                                    )),
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
                                    message: Some(warp_multi_agent_api::message::Message::AgentOutput(
                                        warp_multi_agent_api::message::AgentOutput { text: text.into() },
                                    )),
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

async fn call_backend_with_tools(
    state: &AppState,
    messages: &[serde_json::Value],
) -> Result<LlmResponse, anyhow::Error> {
    let url = state.config.chat_completions_url();
    let model = &state.config.default_model;

    let payload = json!({
        "model": model,
        "messages": messages,
        "tools": openai_tools(),
        "max_tokens": 4096,
        "stream": false
    });

    let resp = state.http
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

    if let Some(tool_calls) = message["tool_calls"].as_array() {
        if !tool_calls.is_empty() {
            tracing::info!(count = tool_calls.len(), "LLM requested tool calls");
            return Ok(LlmResponse::ToolCalls(tool_calls.clone()));
        }
    }

    let text = message["content"].as_str().unwrap_or("(no response)").to_string();
    Ok(LlmResponse::Text(text))
}
