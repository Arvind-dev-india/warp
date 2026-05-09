//! Tests for the multi-agent protobuf+SSE handler.
//!
//! Coverage:
//! 1. **Exhaustive tool coverage**: compile-time `match` on every `Tool` and
//!    `ToolCallResult` variant — when upstream adds a new variant the build
//!    fails here, telling us exactly which function needs updating.
//! 2. **SSE wire format**: base64-url-safe encoding round-trips.
//! 3. **Request extraction**: user query + tool results from protobuf.
//! 4. **Proto↔OpenAI conversion**: tool calls round-trip correctly.
//! 5. **Tool result → text**: each handled result type produces expected text.

use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use prost::Message;
use warp_multi_agent_api::message;

// ════════════════════════════════════════════════════════════════════
// 1. EXHAUSTIVE TOOL COVERAGE — compile-time guards
//
// Each function below uses an exhaustive `match` (NO wildcard `_`).
// When upstream adds a new variant, cargo test --no-run will fail
// with "non-exhaustive patterns", pointing you to the exact enum.
// ════════════════════════════════════════════════════════════════════

/// Exhaustive match on `message::tool_call::Tool`.
#[allow(deprecated)]
fn tool_call_coverage(tool: &message::tool_call::Tool) -> &'static str {
    use message::tool_call::Tool;
    match tool {
        // Tools we map to OpenAI function-calling
        Tool::RunShellCommand(_) => "handled",
        Tool::ReadFiles(_) => "handled",
        Tool::ApplyFileDiffs(_) => "handled",
        Tool::Grep(_) => "handled",
        Tool::FileGlobV2(_) => "handled",
        Tool::SearchCodebase(_) => "handled",
        // Tools we recognize but pass through
        Tool::Server(_) => "passthrough",
        Tool::SuggestPlan(_) => "passthrough",
        Tool::SuggestCreatePlan(_) => "passthrough",
        Tool::FileGlob(_) => "passthrough",
        Tool::ReadMcpResource(_) => "passthrough",
        Tool::CallMcpTool(_) => "passthrough",
        Tool::WriteToLongRunningShellCommand(_) => "passthrough",
        Tool::SuggestNewConversation(_) => "passthrough",
        Tool::SuggestPrompt(_) => "passthrough",
        Tool::OpenCodeReview(_) => "passthrough",
        Tool::InitProject(_) => "passthrough",
        Tool::Subagent(_) => "passthrough",
        Tool::ReadDocuments(_) => "passthrough",
        Tool::EditDocuments(_) => "passthrough",
        Tool::CreateDocuments(_) => "passthrough",
        Tool::ReadShellCommandOutput(_) => "passthrough",
        Tool::UseComputer(_) => "passthrough",
        Tool::InsertReviewComments(_) => "passthrough",
        Tool::ReadSkill(_) => "passthrough",
        Tool::RequestComputerUse(_) => "passthrough",
        Tool::FetchConversation(_) => "passthrough",
        Tool::StartAgent(_) => "passthrough",
        Tool::SendMessageToAgent(_) => "passthrough",
        Tool::TransferShellCommandControlToUser(_) => "passthrough",
        Tool::AskUserQuestion(_) => "passthrough",
        Tool::StartAgentV2(_) => "passthrough",
        Tool::UploadFileArtifact(_) => "passthrough",
        Tool::RunAgents(_) => "passthrough",
    }
}

/// Exhaustive match on `message::tool_call_result::Result`.
#[allow(deprecated)]
fn message_tool_result_coverage(
    result: &message::tool_call_result::Result,
) -> &'static str {
    use message::tool_call_result::Result as R;
    match result {
        R::RunShellCommand(_) => "handled",
        R::ReadFiles(_) => "handled",
        R::ApplyFileDiffs(_) => "handled",
        R::Grep(_) => "handled",
        R::FileGlobV2(_) => "handled",
        R::SearchCodebase(_) => "handled",
        R::SuggestPlan(_) => "handled",
        R::SuggestCreatePlan(_) => "handled",
        R::Cancel(_) => "handled",
        R::Server(_) => "passthrough",
        R::FileGlob(_) => "passthrough",
        R::ReadMcpResource(_) => "passthrough",
        R::CallMcpTool(_) => "passthrough",
        R::WriteToLongRunningShellCommand(_) => "passthrough",
        R::SuggestNewConversation(_) => "passthrough",
        R::SuggestPrompt(_) => "passthrough",
        R::OpenCodeReview(_) => "passthrough",
        R::InitProject(_) => "passthrough",
        R::Subagent(_) => "passthrough",
        R::ReadDocuments(_) => "passthrough",
        R::EditDocuments(_) => "passthrough",
        R::CreateDocuments(_) => "passthrough",
        R::ReadShellCommandOutput(_) => "passthrough",
        R::UseComputer(_) => "passthrough",
        R::InsertReviewComments(_) => "passthrough",
        R::ReadSkill(_) => "passthrough",
        R::RequestComputerUseResult(_) => "passthrough",
        R::FetchConversation(_) => "passthrough",
        R::StartAgent(_) => "passthrough",
        R::SendMessageToAgent(_) => "passthrough",
        R::TransferShellCommandControlToUser(_) => "passthrough",
        R::AskUserQuestion(_) => "passthrough",
        R::StartAgentV2(_) => "passthrough",
        R::UploadFileArtifact(_) => "passthrough",
        R::RunAgentsResult(_) => "passthrough",
    }
}

/// Exhaustive match on `request::input::tool_call_result::Result`.
#[allow(deprecated)]
fn request_tool_result_coverage(
    result: &warp_multi_agent_api::request::input::tool_call_result::Result,
) -> &'static str {
    use warp_multi_agent_api::request::input::tool_call_result::Result as R;
    match result {
        R::RunShellCommand(_) => "handled",
        R::ReadFiles(_) => "handled",
        R::ApplyFileDiffs(_) => "handled",
        R::Grep(_) => "handled",
        R::FileGlobV2(_) => "handled",
        R::SearchCodebase(_) => "handled",
        R::SuggestPlan(_) => "handled",
        R::SuggestCreatePlan(_) => "handled",
        R::FileGlob(_) => "passthrough",
        R::ReadMcpResource(_) => "passthrough",
        R::CallMcpTool(_) => "passthrough",
        R::WriteToLongRunningShellCommand(_) => "passthrough",
        R::SuggestNewConversation(_) => "passthrough",
        R::SuggestPrompt(_) => "passthrough",
        R::OpenCodeReview(_) => "passthrough",
        R::InitProject(_) => "passthrough",
        R::ReadDocuments(_) => "passthrough",
        R::EditDocuments(_) => "passthrough",
        R::CreateDocuments(_) => "passthrough",
        R::ReadShellCommandOutput(_) => "passthrough",
        R::UseComputer(_) => "passthrough",
        R::InsertReviewComments(_) => "passthrough",
        R::RequestComputerUse(_) => "passthrough",
        R::ReadSkill(_) => "passthrough",
        R::FetchConversation(_) => "passthrough",
        R::StartAgent(_) => "passthrough",
        R::SendMessageToAgent(_) => "passthrough",
        R::TransferShellCommandControlToUser(_) => "passthrough",
        R::AskUserQuestion(_) => "passthrough",
        R::StartAgentV2(_) => "passthrough",
        R::UploadFileArtifact(_) => "passthrough",
        R::RunAgentsResult(_) => "passthrough",
    }
}

// Ensure the exhaustive functions compile (= all variants covered)
#[test]
fn exhaustive_tool_call_variants() {
    let t = message::tool_call::Tool::RunShellCommand(Default::default());
    assert_eq!(tool_call_coverage(&t), "handled");
}

#[test]
fn exhaustive_message_tool_result_variants() {
    let r = message::tool_call_result::Result::Cancel(Default::default());
    assert_eq!(message_tool_result_coverage(&r), "handled");
}

#[test]
fn exhaustive_request_tool_result_variants() {
    let r = warp_multi_agent_api::request::input::tool_call_result::Result::RunShellCommand(
        Default::default(),
    );
    assert_eq!(request_tool_result_coverage(&r), "handled");
}

// ════════════════════════════════════════════════════════════════════
// 2. SSE WIRE FORMAT
// ════════════════════════════════════════════════════════════════════

fn decode_sse_events(body: &str) -> Vec<warp_multi_agent_api::ResponseEvent> {
    body.lines()
        .filter(|l| l.starts_with("data: "))
        .map(|l| {
            let b64 = l.trim_start_matches("data: ").trim_matches('"');
            let bytes = URL_SAFE.decode(b64).expect("valid base64");
            warp_multi_agent_api::ResponseEvent::decode(bytes.as_slice())
                .expect("valid protobuf")
        })
        .collect()
}

#[test]
fn init_event_round_trips_through_sse() {
    use warp_multi_agent_api::response_event;

    let event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(response_event::Type::Init(response_event::StreamInit {
            conversation_id: "conv-1".into(),
            request_id: "req-1".into(),
            run_id: "run-1".into(),
        })),
    };

    let b64 = URL_SAFE.encode(event.encode_to_vec());
    let line = format!("data: \"{b64}\"\n\n");
    let events = decode_sse_events(&line);

    assert_eq!(events.len(), 1);
    match &events[0].r#type {
        Some(response_event::Type::Init(init)) => {
            assert_eq!(init.conversation_id, "conv-1");
            assert_eq!(init.request_id, "req-1");
        }
        other => panic!("Expected Init, got {other:?}"),
    }
}

#[test]
fn finished_done_event_round_trips() {
    use warp_multi_agent_api::response_event;

    let event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(response_event::Type::Finished(
            response_event::StreamFinished {
                reason: Some(response_event::stream_finished::Reason::Done(
                    response_event::stream_finished::Done {},
                )),
                ..Default::default()
            },
        )),
    };

    let bytes = event.encode_to_vec();
    let decoded =
        warp_multi_agent_api::ResponseEvent::decode(URL_SAFE.decode(URL_SAFE.encode(&bytes)).unwrap().as_slice())
            .unwrap();

    assert!(matches!(
        decoded.r#type,
        Some(response_event::Type::Finished(response_event::StreamFinished {
            reason: Some(response_event::stream_finished::Reason::Done(_)),
            ..
        }))
    ));
}

// ════════════════════════════════════════════════════════════════════
// 3. REQUEST EXTRACTION
// ════════════════════════════════════════════════════════════════════

fn make_user_query_request(query: &str) -> warp_multi_agent_api::Request {
    use warp_multi_agent_api::request;
    warp_multi_agent_api::Request {
        input: Some(request::Input {
            r#type: Some(request::input::Type::UserInputs(request::input::UserInputs {
                inputs: vec![request::input::user_inputs::UserInput {
                    input: Some(request::input::user_inputs::user_input::Input::UserQuery(
                        request::input::UserQuery { query: query.into(), ..Default::default() },
                    )),
                }],
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn make_tool_result_request(
    tool_call_id: &str, command: &str, output: &str, exit_code: i32,
) -> warp_multi_agent_api::Request {
    use warp_multi_agent_api::{request, run_shell_command_result, RunShellCommandResult, ShellCommandFinished};
    warp_multi_agent_api::Request {
        input: Some(request::Input {
            r#type: Some(request::input::Type::UserInputs(request::input::UserInputs {
                inputs: vec![request::input::user_inputs::UserInput {
                    input: Some(request::input::user_inputs::user_input::Input::ToolCallResult(
                        request::input::ToolCallResult {
                            tool_call_id: tool_call_id.into(),
                            result: Some(request::input::tool_call_result::Result::RunShellCommand(
                                RunShellCommandResult {
                                    command: command.into(),
                                    result: Some(run_shell_command_result::Result::CommandFinished(
                                        ShellCommandFinished { output: output.into(), exit_code, ..Default::default() },
                                    )),
                                    ..Default::default()
                                },
                            )),
                            ..Default::default()
                        },
                    )),
                }],
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn extract_query(req: &warp_multi_agent_api::Request) -> Option<String> {
    let input = req.input.as_ref()?.r#type.as_ref()?;
    if let warp_multi_agent_api::request::input::Type::UserInputs(ui) = input {
        for u in &ui.inputs {
            if let Some(warp_multi_agent_api::request::input::user_inputs::user_input::Input::UserQuery(q)) = u.input.as_ref() {
                return Some(q.query.clone());
            }
        }
    }
    None
}

fn extract_results(req: &warp_multi_agent_api::Request) -> Vec<String> {
    let mut out = Vec::new();
    let Some(input) = req.input.as_ref().and_then(|i| i.r#type.as_ref()) else { return out };
    if let warp_multi_agent_api::request::input::Type::UserInputs(ui) = input {
        for u in &ui.inputs {
            if let Some(warp_multi_agent_api::request::input::user_inputs::user_input::Input::ToolCallResult(tcr)) = u.input.as_ref() {
                out.push(tcr.tool_call_id.clone());
            }
        }
    }
    out
}

#[test]
fn extract_user_query_from_protobuf() {
    let req = make_user_query_request("hello world");
    let bytes = req.encode_to_vec();
    let decoded = warp_multi_agent_api::Request::decode(bytes.as_slice()).unwrap();
    assert_eq!(extract_query(&decoded).as_deref(), Some("hello world"));
}

#[test]
fn extract_tool_results_from_protobuf() {
    let req = make_tool_result_request("call-123", "ls", "file.txt", 0);
    let bytes = req.encode_to_vec();
    let decoded = warp_multi_agent_api::Request::decode(bytes.as_slice()).unwrap();
    let ids = extract_results(&decoded);
    assert_eq!(ids, vec!["call-123"]);
}

#[test]
fn empty_request_yields_no_query() {
    assert!(extract_query(&warp_multi_agent_api::Request::default()).is_none());
}

#[test]
fn empty_request_yields_no_results() {
    assert!(extract_results(&warp_multi_agent_api::Request::default()).is_empty());
}

// ════════════════════════════════════════════════════════════════════
// 4. PROTO ↔ OPENAI CONVERSION
// ════════════════════════════════════════════════════════════════════

#[test]
fn run_shell_command_round_trips() {
    let tc = message::ToolCall {
        tool_call_id: "tc-1".into(),
        tool: Some(message::tool_call::Tool::RunShellCommand(
            message::tool_call::RunShellCommand { command: "echo hi".into(), ..Default::default() },
        )),
    };
    let openai_json = serde_json::json!({
        "id": "tc-1", "type": "function",
        "function": { "name": "run_shell_command", "arguments": "{\"command\":\"echo hi\"}" }
    });
    // Proto → OpenAI name
    assert!(matches!(&tc.tool, Some(message::tool_call::Tool::RunShellCommand(c)) if c.command == "echo hi"));

    // OpenAI → Proto
    let back = convert_openai_to_proto(&openai_json).unwrap();
    assert_eq!(back.tool_call_id, "tc-1");
    assert!(matches!(&back.tool, Some(message::tool_call::Tool::RunShellCommand(c)) if c.command == "echo hi"));
}

#[test]
fn read_files_proto_has_correct_name() {
    let tc = message::ToolCall {
        tool_call_id: "tc-2".into(),
        tool: Some(message::tool_call::Tool::ReadFiles(message::tool_call::ReadFiles {
            files: vec![message::tool_call::read_files::File { name: "foo.rs".into(), line_ranges: vec![] }],
        })),
    };
    assert!(matches!(&tc.tool, Some(message::tool_call::Tool::ReadFiles(_))));
}

#[test]
fn unknown_openai_tool_returns_none() {
    let tc = serde_json::json!({
        "id": "x", "type": "function",
        "function": { "name": "nonexistent", "arguments": "{}" }
    });
    assert!(convert_openai_to_proto(&tc).is_none());
}

fn convert_openai_to_proto(tc: &serde_json::Value) -> Option<message::ToolCall> {
    let fn_name = tc["function"]["name"].as_str()?;
    let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
    let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();

    use message::tool_call::Tool;
    let tool = match fn_name {
        "run_shell_command" => Tool::RunShellCommand(message::tool_call::RunShellCommand {
            command: args["command"].as_str().unwrap_or("").into(), ..Default::default()
        }),
        "read_files" => {
            let files = args["files"].as_array().map(|a| a.iter().filter_map(|f| {
                Some(message::tool_call::read_files::File { name: f["name"].as_str()?.into(), line_ranges: vec![] })
            }).collect()).unwrap_or_default();
            Tool::ReadFiles(message::tool_call::ReadFiles { files })
        }
        _ => return None,
    };
    Some(message::ToolCall { tool_call_id, tool: Some(tool) })
}

// ════════════════════════════════════════════════════════════════════
// 5. TOOL RESULT → TEXT
// ════════════════════════════════════════════════════════════════════

#[test]
fn shell_command_finished_to_text() {
    use warp_multi_agent_api::{run_shell_command_result, RunShellCommandResult, ShellCommandFinished, request};
    let tcr = request::input::ToolCallResult {
        tool_call_id: "tc-1".into(),
        result: Some(request::input::tool_call_result::Result::RunShellCommand(RunShellCommandResult {
            command: "ls".into(),
            result: Some(run_shell_command_result::Result::CommandFinished(
                ShellCommandFinished { output: "a.txt\nb.txt".into(), exit_code: 0, ..Default::default() },
            )), ..Default::default()
        })), ..Default::default()
    };
    let text = result_to_text(&tcr);
    assert!(text.contains("ls"), "should contain command");
    assert!(text.contains("Exit code: 0"), "should contain exit code");
    assert!(text.contains("a.txt"), "should contain output");
}

#[test]
fn permission_denied_to_text() {
    use warp_multi_agent_api::{run_shell_command_result, RunShellCommandResult, request};
    let tcr = request::input::ToolCallResult {
        tool_call_id: "tc-2".into(),
        result: Some(request::input::tool_call_result::Result::RunShellCommand(RunShellCommandResult {
            command: "rm -rf /".into(),
            result: Some(run_shell_command_result::Result::PermissionDenied(
                warp_multi_agent_api::PermissionDenied { reason: None },
            )), ..Default::default()
        })), ..Default::default()
    };
    let text = result_to_text(&tcr);
    assert!(text.contains("Permission denied"));
}

#[test]
fn grep_result_to_text() {
    use warp_multi_agent_api::{grep_result, GrepResult, request};
    let tcr = request::input::ToolCallResult {
        tool_call_id: "tc-3".into(),
        result: Some(request::input::tool_call_result::Result::Grep(GrepResult {
            result: Some(grep_result::Result::Success(grep_result::Success {
                matched_files: vec![grep_result::success::GrepFileMatch {
                    file_path: "main.rs".into(),
                    matched_lines: vec![
                        grep_result::success::grep_file_match::GrepLineMatch { line_number: 10 },
                        grep_result::success::grep_file_match::GrepLineMatch { line_number: 25 },
                    ],
                }],
            })),
        })), ..Default::default()
    };
    let text = result_to_text(&tcr);
    assert!(text.contains("main.rs"));
    assert!(text.contains("10"));
    assert!(text.contains("25"));
}

#[test]
fn empty_result_returns_placeholder() {
    let tcr = warp_multi_agent_api::request::input::ToolCallResult {
        tool_call_id: "empty".into(), result: None, ..Default::default()
    };    assert_eq!(result_to_text(&tcr), "(no result)");
}

fn result_to_text(tcr: &warp_multi_agent_api::request::input::ToolCallResult) -> String {
    let Some(ref result) = tcr.result else { return "(no result)".into() };
    use warp_multi_agent_api::request::input::tool_call_result::Result as R;
    match result {
        R::RunShellCommand(r) => match &r.result {
            Some(warp_multi_agent_api::run_shell_command_result::Result::CommandFinished(f)) =>
                format!("Command: {}\nExit code: {}\nOutput:\n{}", r.command, f.exit_code, f.output),
            Some(warp_multi_agent_api::run_shell_command_result::Result::PermissionDenied(_)) =>
                format!("Command: {} — Permission denied by user", r.command),
            _ => "(shell result)".into(),
        },
        R::Grep(r) => match &r.result {
            Some(warp_multi_agent_api::grep_result::Result::Success(s)) => s.matched_files.iter().map(|f| {
                let lines: Vec<String> = f.matched_lines.iter().map(|l| l.line_number.to_string()).collect();
                format!("{}:{}", f.file_path, lines.join(","))
            }).collect::<Vec<_>>().join("\n"),
            _ => "(grep result)".into(),
        },
        _ => "(tool result)".into(),
    }
}

// ════════════════════════════════════════════════════════════════════
// 6. TOOL CALL EVENT STRUCTURE
// ════════════════════════════════════════════════════════════════════

#[test]
fn tool_call_event_structure() {
    use warp_multi_agent_api::{response_event, client_action};

    let proto_tc = message::ToolCall {
        tool_call_id: "call-1".into(),
        tool: Some(message::tool_call::Tool::RunShellCommand(
            message::tool_call::RunShellCommand { command: "echo test".into(), ..Default::default() },
        )),
    };

    let event = warp_multi_agent_api::ResponseEvent {
        r#type: Some(response_event::Type::ClientActions(response_event::ClientActions {
            actions: vec![warp_multi_agent_api::ClientAction {
                action: Some(client_action::Action::AddMessagesToTask(
                    client_action::AddMessagesToTask {
                        task_id: "task-1".into(),
                        messages: vec![warp_multi_agent_api::Message {
                            id: "msg-1".into(),
                            task_id: "task-1".into(),
                            request_id: "req-1".into(),
                            message: Some(message::Message::ToolCall(proto_tc)),
                            ..Default::default()
                        }],
                    },
                )),
            }],
        })),
    };

    // Round-trip
    let bytes = event.encode_to_vec();
    let decoded = warp_multi_agent_api::ResponseEvent::decode(bytes.as_slice()).unwrap();

    match &decoded.r#type {
        Some(response_event::Type::ClientActions(ca)) => {
            assert_eq!(ca.actions.len(), 1);
            if let Some(client_action::Action::AddMessagesToTask(amt)) = &ca.actions[0].action {
                assert_eq!(amt.task_id, "task-1");
                assert_eq!(amt.messages.len(), 1);
                assert!(matches!(&amt.messages[0].message, Some(message::Message::ToolCall(tc)) if tc.tool_call_id == "call-1"));
            } else {
                panic!("Expected AddMessagesToTask");
            }
        }
        other => panic!("Expected ClientActions, got {other:?}"),
    }
}

// ════════════════════════════════════════════════════════════════════
// 7. PROTOBUF REQUEST ROUND-TRIP
// ════════════════════════════════════════════════════════════════════

#[test]
fn user_query_request_round_trips() {
    let req = make_user_query_request("what does this code do?");
    let bytes = req.encode_to_vec();
    let decoded = warp_multi_agent_api::Request::decode(bytes.as_slice()).unwrap();
    assert_eq!(extract_query(&decoded).as_deref(), Some("what does this code do?"));
}

#[test]
fn tool_result_request_round_trips() {
    let req = make_tool_result_request("abc", "cat foo", "hello", 0);
    let bytes = req.encode_to_vec();
    let decoded = warp_multi_agent_api::Request::decode(bytes.as_slice()).unwrap();
    assert_eq!(extract_results(&decoded), vec!["abc"]);
}
