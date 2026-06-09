use super::{
    api_keys_with_warp_credit_fallback_setting, get_supported_cli_agent_tools, get_supported_tools,
};
use crate::ai::agent::api::RequestParams;
use crate::ai::agent::conversation::AIConversationId;
use crate::ai::blocklist::SessionContext;
use crate::ai::llms::LLMId;
use crate::terminal::model::session::SessionType;
use serde_json::Value;
use std::sync::Arc;
use warp_core::command::ExitCode;
use warp_core::features::FeatureFlag;
use warp_core::HostId;
use warp_multi_agent_api as api;

fn request_params_with_ask_user_question_enabled(ask_user_question_enabled: bool) -> RequestParams {
    let model = LLMId::from("test-model");

    RequestParams {
        input: vec![],
        input_task_id: None,
        local_conversation_id: AIConversationId::new(),
        conversation_token: None,
        forked_from_conversation_token: None,
        ambient_agent_task_id: None,
        tasks: vec![],
        existing_suggestions: None,
        metadata: None,
        session_context: SessionContext::new_for_test(),
        model: model.clone(),
        coding_model: model.clone(),
        cli_agent_model: model.clone(),
        computer_use_model: model,
        is_memory_enabled: false,
        warp_drive_context_enabled: false,
        context_window_limit: None,
        mcp_context: None,
        planning_enabled: true,
        should_redact_secrets: false,
        api_keys: None,
        custom_model_providers: None,
        allow_use_of_warp_credits: false,
        autonomy_level: api::AutonomyLevel::Supervised,
        isolation_level: api::IsolationLevel::None,
        web_search_enabled: false,
        computer_use_enabled: false,
        ask_user_question_enabled,
        research_agent_enabled: false,
        orchestration_enabled: false,
        supported_tools_override: None,
        parent_agent_id: None,
        agent_name: None,
    }
}

fn request_params_for_remote(host_id: Option<HostId>) -> RequestParams {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.session_context =
        SessionContext::new_with_session_type_for_test(Some(SessionType::WarpifiedRemote {
            host_id,
        }));
    params
}

fn local_tool_names(params: &RequestParams) -> Vec<&'static str> {
    super::local_openai_tools(params)
        .into_iter()
        .map(|tool| tool.function.name)
        .collect()
}

fn has_continue_after_tool_result_instruction(messages: &[super::ChatCompletionMessage]) -> bool {
    messages.iter().any(|message| {
        message.role == "system"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.starts_with("You received a tool result."))
    })
}

fn has_system_content_containing(messages: &[super::ChatCompletionMessage], needle: &str) -> bool {
    messages.iter().any(|message| {
        message.role == "system"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains(needle))
    })
}

fn api_user_query_message(task_id: &str, request_id: &str, query: &str) -> api::Message {
    api::Message {
        id: format!("{request_id}-user"),
        task_id: task_id.to_string(),
        server_message_data: String::new(),
        citations: vec![],
        message: Some(api::message::Message::UserQuery(api::message::UserQuery {
            query: query.to_string(),
            context: None,
            referenced_attachments: Default::default(),
            mode: None,
            intended_agent: Default::default(),
        })),
        request_id: request_id.to_string(),
        timestamp: None,
    }
}

fn api_agent_output_message(task_id: &str, request_id: &str, text: &str) -> api::Message {
    api::Message {
        id: format!("{request_id}-assistant"),
        task_id: task_id.to_string(),
        server_message_data: String::new(),
        citations: vec![],
        message: Some(api::message::Message::AgentOutput(
            api::message::AgentOutput {
                text: text.to_string(),
            },
        )),
        request_id: request_id.to_string(),
        timestamp: None,
    }
}

#[test]
fn api_keys_with_warp_credit_fallback_setting_returns_none_without_keys_or_fallback() {
    let api_keys = api_keys_with_warp_credit_fallback_setting(None, false);

    assert!(api_keys.is_none());
}

#[test]
fn api_keys_with_warp_credit_fallback_setting_creates_fallback_only_api_keys() {
    let api_keys = api_keys_with_warp_credit_fallback_setting(None, true)
        .expect("fallback setting should create ApiKeys");

    assert!(api_keys.allow_use_of_warp_credits);
    assert!(api_keys.anthropic.is_empty());
    assert!(api_keys.openai.is_empty());
    assert!(api_keys.google.is_empty());
    assert!(api_keys.open_router.is_empty());
    assert!(api_keys.aws_credentials.is_none());
}

#[test]
fn api_keys_with_warp_credit_fallback_setting_preserves_existing_keys() {
    let api_keys = api_keys_with_warp_credit_fallback_setting(
        Some(api::request::settings::ApiKeys {
            anthropic: "anthropic-key".to_string(),
            openai: String::new(),
            google: String::new(),
            open_router: String::new(),
            allow_use_of_warp_credits: false,
            aws_credentials: None,
        }),
        true,
    )
    .expect("existing ApiKeys should be preserved");

    assert_eq!(api_keys.anthropic, "anthropic-key");
    assert!(api_keys.allow_use_of_warp_credits);
}
#[test]
fn supported_tools_omits_ask_user_question_when_disabled() {
    let params = request_params_with_ask_user_question_enabled(false);
    let supported_tools = get_supported_tools(&params);

    assert!(!supported_tools.contains(&api::ToolType::AskUserQuestion));
}

#[test]
fn supported_tools_includes_ask_user_question_when_enabled_and_feature_flag_is_enabled() {
    if !FeatureFlag::AskUserQuestion.is_enabled() {
        return;
    }

    let params = request_params_with_ask_user_question_enabled(true);
    let supported_tools = get_supported_tools(&params);

    assert!(supported_tools.contains(&api::ToolType::AskUserQuestion));
}

#[test]
fn supported_tools_include_upload_artifact_when_feature_flag_is_enabled() {
    let _flag = FeatureFlag::ArtifactCommand.override_enabled(true);
    let params = request_params_with_ask_user_question_enabled(false);
    let supported_tools = get_supported_tools(&params);

    assert!(supported_tools.contains(&api::ToolType::UploadFileArtifact));
}

#[test]
fn supported_tools_omit_upload_artifact_when_feature_flag_is_disabled() {
    let _flag = FeatureFlag::ArtifactCommand.override_enabled(false);
    let params = request_params_with_ask_user_question_enabled(false);
    let supported_tools = get_supported_tools(&params);

    assert!(!supported_tools.contains(&api::ToolType::UploadFileArtifact));
}

#[test]
fn remote_supported_tools_include_search_codebase_when_connected_and_feature_flag_is_enabled() {
    let _flag = FeatureFlag::RemoteCodebaseIndexing.override_enabled(true);
    let params = request_params_for_remote(Some(HostId::new("host".to_string())));
    let supported_tools = get_supported_tools(&params);
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);

    assert!(supported_tools.contains(&api::ToolType::SearchCodebase));
    assert!(supported_cli_agent_tools.contains(&api::ToolType::SearchCodebase));
}
#[test]
fn remote_supported_tools_omit_search_codebase_when_feature_flag_is_disabled() {
    let _flag = FeatureFlag::RemoteCodebaseIndexing.override_enabled(false);
    let params = request_params_for_remote(Some(HostId::new("host".to_string())));
    let supported_tools = get_supported_tools(&params);
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);

    assert!(!supported_tools.contains(&api::ToolType::SearchCodebase));
    assert!(!supported_cli_agent_tools.contains(&api::ToolType::SearchCodebase));
}

#[test]
fn remote_supported_tools_omit_search_codebase_when_remote_is_not_connected() {
    let _flag = FeatureFlag::RemoteCodebaseIndexing.override_enabled(true);
    let params = request_params_for_remote(None);
    let supported_tools = get_supported_tools(&params);
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);

    assert!(!supported_tools.contains(&api::ToolType::SearchCodebase));
    assert!(!supported_cli_agent_tools.contains(&api::ToolType::SearchCodebase));
}

#[test]
fn local_openai_tools_include_implemented_default_local_tools() {
    let params = request_params_with_ask_user_question_enabled(false);
    let names = local_tool_names(&params);

    for expected in [
        "run_shell_command",
        "write_to_long_running_shell_command",
        "read_shell_command_output",
        "read_files",
        "grep",
        "file_glob_v2",
        "search_codebase",
    ] {
        assert!(
            names.contains(&expected),
            "expected local tool schema for {expected}"
        );
    }

    assert!(!names.contains(&"apply_file_diffs"));
    assert!(!names.contains(&"ask_user_question"));
}

#[test]
fn local_openai_tools_respect_supported_tools_override_for_main_tools() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.supported_tools_override = Some(vec![api::ToolType::Grep]);
    let names = local_tool_names(&params);

    assert!(names.contains(&"grep"));
    assert!(!names.contains(&"run_shell_command"));
    assert!(!names.contains(&"read_files"));
    assert!(!names.contains(&"search_codebase"));
}

#[test]
fn local_openai_tools_omit_file_tools_when_remote_is_not_connected() {
    let _flag = FeatureFlag::RemoteCodebaseIndexing.override_enabled(true);
    let params = request_params_for_remote(None);
    let names = local_tool_names(&params);

    assert!(names.contains(&"run_shell_command"));
    assert!(names.contains(&"grep"));
    assert!(!names.contains(&"read_files"));
    assert!(!names.contains(&"search_codebase"));
}

#[test]
fn local_openai_tools_gate_transfer_control_with_feature_flag() {
    let params = request_params_with_ask_user_question_enabled(false);

    {
        let _flag = FeatureFlag::TransferControlTool.override_enabled(true);
        let names = local_tool_names(&params);
        assert!(names.contains(&"transfer_shell_command_control_to_user"));
    }

    {
        let _flag = FeatureFlag::TransferControlTool.override_enabled(false);
        let names = local_tool_names(&params);
        assert!(!names.contains(&"transfer_shell_command_control_to_user"));
    }
}

#[test]
fn test_parse_dsml_tool_calls_unicode_pipe_does_not_panic() {
    // Regression test: DSML contains multi-byte characters like `｜` and must never use byte slicing.
    let content = r#"<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="read_shell_command_output">
<｜｜DSML｜｜parameter name="command_id" string="true">precmd-123</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>"#;

    // Access via super::tests module boundary is fine because this file is compiled as a sibling module to impl.rs.
    let calls = super::parse_dsml_tool_calls(content).expect("should parse DSML tool calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "read_shell_command_output");

    let args: Value = serde_json::from_str(&calls[0].function.arguments).expect("valid JSON args");
    assert_eq!(
        args.get("command_id").and_then(Value::as_str),
        Some("precmd-123")
    );
}

#[test]
fn test_actionresult_with_user_query_appends_user_prompt_without_continuation_instruction() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-actionresult-with-user-query".to_string(),
    ));
    params.input = vec![
        crate::ai::agent::AIAgentInput::UserQuery {
            query: "check memory".to_string(),
            context: Arc::from([]),
            static_query_type: None,
            referenced_attachments: Default::default(),
            user_query_mode: crate::ai::agent::UserQueryMode::Normal,
            running_command: None,
            intended_agent: None,
        },
        crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from("call_1".to_string()),
                task_id: crate::ai::agent::task::TaskId::new("task".to_string()),
                result: crate::ai::agent::AIAgentActionResultType::ReadShellCommandOutput(
                    crate::ai::agent::ReadShellCommandOutputResult::Cancelled,
                ),
            },
            context: Arc::from([]),
        },
    ];

    let messages = super::local_openai_messages(&params);
    assert!(messages.iter().any(|message| {
        message.role == "user"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("check memory"))
    }));
    assert!(!has_continue_after_tool_result_instruction(&messages));
}

#[test]
fn test_local_openai_messages_restores_persisted_task_messages() {
    let task_id = format!("test-restored-local-{}", uuid::Uuid::new_v4());

    let mut params = request_params_with_ask_user_question_enabled(false);
    let conversation_key = params.local_conversation_id.to_string();
    super::LOCAL_CONVERSATIONS.lock().remove(&conversation_key);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(task_id.clone()));
    params.tasks = vec![api::Task {
        id: task_id.clone(),
        messages: vec![
            api_user_query_message(&task_id, "request-1", "first local question"),
            api_agent_output_message(&task_id, "request-1", "first local answer"),
        ],
        dependencies: None,
        description: String::new(),
        summary: String::new(),
        server_data: String::new(),
    }];
    params.input = vec![crate::ai::agent::AIAgentInput::UserQuery {
        query: "continue locally".to_string(),
        context: Arc::from([]),
        static_query_type: None,
        referenced_attachments: Default::default(),
        user_query_mode: crate::ai::agent::UserQueryMode::Normal,
        running_command: None,
        intended_agent: None,
    }];

    let messages = super::local_openai_messages(&params);

    assert!(messages.iter().any(|message| {
        message.role == "user"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("first local question"))
    }));
    assert!(messages.iter().any(|message| {
        message.role == "assistant" && message.content.as_deref() == Some("first local answer")
    }));
    assert!(messages.iter().any(|message| {
        message.role == "user"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("continue locally"))
    }));

    super::LOCAL_CONVERSATIONS.lock().remove(&conversation_key);
}

#[test]
fn test_actionresult_only_request_keeps_tools_available_with_continuation_instruction() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-actionresult-only-continuation".to_string(),
    ));
    params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
        result: crate::ai::agent::AIAgentActionResult {
            id: crate::ai::agent::AIAgentActionId::from("call_2".to_string()),
            task_id: crate::ai::agent::task::TaskId::new("task".to_string()),
            result: crate::ai::agent::AIAgentActionResultType::ReadShellCommandOutput(
                crate::ai::agent::ReadShellCommandOutputResult::Cancelled,
            ),
        },
        context: Arc::from([]),
    }];

    let messages = super::local_openai_messages(&params);
    assert!(has_continue_after_tool_result_instruction(&messages));
    assert!(!has_system_content_containing(
        &messages,
        "Do not call any more tools for this turn"
    ));
}

#[test]
fn test_snapshot_actionresult_continues_without_read_only_constraint() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-snapshot-actionresult-continuation".to_string(),
    ));
    params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
        result: crate::ai::agent::AIAgentActionResult {
            id: crate::ai::agent::AIAgentActionId::from("call_snapshot".to_string()),
            task_id: crate::ai::agent::task::TaskId::new("task".to_string()),
            result: crate::ai::agent::AIAgentActionResultType::WriteToLongRunningShellCommand(
                crate::ai::agent::WriteToLongRunningShellCommandResult::Snapshot {
                    block_id: crate::terminal::model::block::BlockId::new(),
                    grid_contents: "partial output".to_string(),
                    cursor: "CURSOR".to_string(),
                    is_alt_screen_active: false,
                    is_preempted: false,
                },
            ),
        },
        context: Arc::from([]),
    }];

    let messages = super::local_openai_messages(&params);
    assert!(has_continue_after_tool_result_instruction(&messages));
    assert!(!has_system_content_containing(
        &messages,
        "Do not call write_to_long_running_shell_command again for this turn"
    ));
}

#[test]
fn test_successful_run_shell_actionresult_continues_after_tool_result() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-successful-followup".to_string(),
    ));
    params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
        result: crate::ai::agent::AIAgentActionResult {
            id: crate::ai::agent::AIAgentActionId::from("call_public_ip".to_string()),
            task_id: crate::ai::agent::task::TaskId::new("test-successful-followup".to_string()),
            result: crate::ai::agent::AIAgentActionResultType::RequestCommandOutput(
                crate::ai::agent::RequestCommandOutputResult::Completed {
                    block_id: crate::terminal::model::block::BlockId::new(),
                    command: "curl -s ifconfig.me".to_string(),
                    output: "IPv6: 2603:c021:8020:fb01::7e97".to_string(),
                    exit_code: ExitCode::from(0),
                    start_ts: None,
                    completed_ts: None,
                },
            ),
        },
        context: Arc::from([]),
    }];

    let messages = super::local_openai_messages(&params);
    assert!(has_continue_after_tool_result_instruction(&messages));
}

#[test]
fn test_local_custom_successful_run_shell_followup_is_not_bounded_to_two_rounds() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-unbounded-followup".to_string(),
    ));

    for (id, command, output) in [
        ("call_1", "df -h", "disk ok"),
        ("call_2", "free -h", "memory ok"),
        (
            "call_3",
            "systemctl list-units --type=service",
            "services ok",
        ),
        ("call_4", "docker ps", "containers ok"),
    ] {
        params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from(id.to_string()),
                task_id: crate::ai::agent::task::TaskId::new("test-unbounded-followup".to_string()),
                result: crate::ai::agent::AIAgentActionResultType::RequestCommandOutput(
                    crate::ai::agent::RequestCommandOutputResult::Completed {
                        block_id: crate::terminal::model::block::BlockId::new(),
                        command: command.to_string(),
                        output: output.to_string(),
                        exit_code: ExitCode::from(0),
                        start_ts: None,
                        completed_ts: None,
                    },
                ),
            },
            context: Arc::from([]),
        }];
        let messages = super::local_openai_messages(&params);
        assert!(has_continue_after_tool_result_instruction(&messages));
        assert!(!has_system_content_containing(
            &messages,
            "Do not call any more tools for this turn"
        ));
    }
}

#[test]
fn test_local_custom_repeated_tool_result_keeps_following_up() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-repeated-result".to_string(),
    ));

    for id in ["call_1", "call_2"] {
        params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from(id.to_string()),
                task_id: crate::ai::agent::task::TaskId::new("test-repeated-result".to_string()),
                result: crate::ai::agent::AIAgentActionResultType::RequestCommandOutput(
                    crate::ai::agent::RequestCommandOutputResult::Completed {
                        block_id: crate::terminal::model::block::BlockId::new(),
                        command: "df -h".to_string(),
                        output: "same output".to_string(),
                        exit_code: ExitCode::from(0),
                        start_ts: None,
                        completed_ts: None,
                    },
                ),
            },
            context: Arc::from([]),
        }];
        let _ = super::local_openai_messages(&params);
    }

    let messages = super::local_openai_messages(&params);
    assert!(has_continue_after_tool_result_instruction(&messages));
    assert!(!has_system_content_containing(
        &messages,
        "repeated previous output"
    ));
    assert!(!has_system_content_containing(
        &messages,
        "Do not call any more tools for this turn"
    ));
}

#[test]
fn test_local_custom_no_progress_results_keep_following_up() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-no-progress".to_string(),
    ));

    for index in 0..4 {
        params.input = vec![crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from(format!("call_{index}")),
                task_id: crate::ai::agent::task::TaskId::new("test-no-progress".to_string()),
                result: crate::ai::agent::AIAgentActionResultType::RequestCommandOutput(
                    crate::ai::agent::RequestCommandOutputResult::Denylisted {
                        command: format!("blocked-{index}"),
                    },
                ),
            },
            context: Arc::from([]),
        }];
        let _ = super::local_openai_messages(&params);
    }

    let messages = super::local_openai_messages(&params);
    assert!(has_continue_after_tool_result_instruction(&messages));
    assert!(!has_system_content_containing(
        &messages,
        "did not make progress"
    ));
    assert!(!has_system_content_containing(
        &messages,
        "Do not call any more tools for this turn"
    ));
}

#[test]
fn test_local_openai_messages_appends_user_query_when_action_result_is_present() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-action-result-with-query".to_string(),
    ));

    let setup_call = super::OpenAIToolCall {
        id: "call_memory".to_string(),
        r#type: "function".to_string(),
        function: super::OpenAIToolCallFunction {
            name: "read_shell_command_output".to_string(),
            arguments: "{}".to_string(),
        },
    };
    super::remember_assistant_tool_calls(&params, None, None, std::slice::from_ref(&setup_call));

    params.input = vec![
        crate::ai::agent::AIAgentInput::UserQuery {
            query: "查看内存大小".to_string(),
            context: Arc::from([]),
            static_query_type: None,
            referenced_attachments: Default::default(),
            user_query_mode: crate::ai::agent::UserQueryMode::Normal,
            running_command: None,
            intended_agent: None,
        },
        crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from("call_memory".to_string()),
                task_id: crate::ai::agent::task::TaskId::new(
                    "test-action-result-with-query".to_string(),
                ),
                result: crate::ai::agent::AIAgentActionResultType::ReadShellCommandOutput(
                    crate::ai::agent::ReadShellCommandOutputResult::Cancelled,
                ),
            },
            context: Arc::from([]),
        },
    ];

    let messages = super::local_openai_messages(&params);

    assert!(messages.iter().any(|message| {
        message.role == "tool"
            && message.tool_call_id.as_deref() == Some("call_memory")
            && message.content.is_some()
    }));
    assert!(messages.iter().any(|message| {
        message.role == "user"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("查看内存大小"))
    }));
}

#[test]
fn test_local_openai_messages_appends_user_query_with_orphan_action_result() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-orphan-action-result-with-query".to_string(),
    ));
    params.input = vec![
        crate::ai::agent::AIAgentInput::UserQuery {
            query: "查询内存".to_string(),
            context: Arc::from([]),
            static_query_type: None,
            referenced_attachments: Default::default(),
            user_query_mode: crate::ai::agent::UserQueryMode::Normal,
            running_command: None,
            intended_agent: None,
        },
        crate::ai::agent::AIAgentInput::ActionResult {
            result: crate::ai::agent::AIAgentActionResult {
                id: crate::ai::agent::AIAgentActionId::from("orphan_call".to_string()),
                task_id: crate::ai::agent::task::TaskId::new(
                    "test-orphan-action-result-with-query".to_string(),
                ),
                result: crate::ai::agent::AIAgentActionResultType::ReadShellCommandOutput(
                    crate::ai::agent::ReadShellCommandOutputResult::Cancelled,
                ),
            },
            context: Arc::from([]),
        },
    ];

    let messages = super::local_openai_messages(&params);
    assert!(messages.iter().any(|message| {
        message.role == "user"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("查询内存"))
    }));
}

#[test]
fn test_tool_result_uses_markdown_action_result_expands_snapshot_grid() {
    // Ensure we pass real terminal output to the model (not "Sent snapshot ...").
    let result = crate::ai::agent::AIAgentActionResultType::WriteToLongRunningShellCommand(
        crate::ai::agent::WriteToLongRunningShellCommandResult::Snapshot {
            block_id: crate::terminal::model::block::BlockId::new(),
            grid_contents: "Filesystem  Size Used Avail Use% Mounted on".to_string(),
            cursor: "CURSOR".to_string(),
            is_alt_screen_active: false,
            is_preempted: false,
        },
    );
    let rendered = format!("{}", crate::ai::agent::MarkdownActionResult(&result));
    assert!(rendered.contains("Filesystem"));
}

#[test]
fn test_write_to_long_running_shell_command_defaults_to_line_mode() {
    // Regression: when the model omits mode, we must default to "line" so the PTY input executes.
    let call = super::OpenAIToolCall {
        id: "call_1".to_string(),
        r#type: "function".to_string(),
        function: super::OpenAIToolCallFunction {
            name: "write_to_long_running_shell_command".to_string(),
            arguments: serde_json::json!({"input":"df -h"}).to_string(),
        },
    };

    let msg = super::warp_tool_message_from_openai_tool_call(
        &call,
        "task",
        "req",
        Some("precmd-1"),
        false,
    )
    .expect("tool call should convert");
    let tool_call = match msg.message.expect("message") {
        warp_multi_agent_api::message::Message::ToolCall(tc) => tc,
        other => panic!("expected ToolCall, got {other:?}"),
    };
    let tool = tool_call.tool.expect("tool");
    let w = match tool {
        warp_multi_agent_api::message::tool_call::Tool::WriteToLongRunningShellCommand(w) => w,
        other => panic!("expected WriteToLongRunningShellCommand, got {other:?}"),
    };
    let mode = w.mode.expect("mode").mode.expect("mode inner");
    match mode {
        warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::mode::Mode::Line(()) => {}
        other => panic!("expected line mode, got {other:?}"),
    }
}

#[test]
fn test_write_to_long_running_shell_command_enter_mode_maps_to_line_mode() {
    let call = super::OpenAIToolCall {
        id: "call_1".to_string(),
        r#type: "function".to_string(),
        function: super::OpenAIToolCallFunction {
            name: "write_to_long_running_shell_command".to_string(),
            arguments: serde_json::json!({"input":"df -h", "mode": "enter"}).to_string(),
        },
    };

    let msg = super::warp_tool_message_from_openai_tool_call(
        &call,
        "task",
        "req",
        Some("precmd-1"),
        false,
    )
    .expect("tool call should convert");
    let tool_call = match msg.message.expect("message") {
        warp_multi_agent_api::message::Message::ToolCall(tc) => tc,
        other => panic!("expected ToolCall, got {other:?}"),
    };
    let tool = tool_call.tool.expect("tool");
    let w = match tool {
        warp_multi_agent_api::message::tool_call::Tool::WriteToLongRunningShellCommand(w) => w,
        other => panic!("expected WriteToLongRunningShellCommand, got {other:?}"),
    };
    let mode = w.mode.expect("mode").mode.expect("mode inner");
    match mode {
        warp_multi_agent_api::message::tool_call::write_to_long_running_shell_command::mode::Mode::Line(()) => {}
        other => panic!("expected line mode, got {other:?}"),
    }
}

#[test]
fn test_read_only_followup_rejects_non_read_only_run_shell_command() {
    let call = super::OpenAIToolCall {
        id: "call_1".to_string(),
        r#type: "function".to_string(),
        function: super::OpenAIToolCallFunction {
            name: "run_shell_command".to_string(),
            arguments: serde_json::json!({
                "command": "rm -rf /tmp/example",
                "is_read_only": false,
                "is_risky": true
            })
            .to_string(),
        },
    };

    let err = super::warp_tool_message_from_openai_tool_call(&call, "task", "req", None, true)
        .expect_err("read-only follow-up should reject risky shell commands");
    assert!(err.contains("is_read_only=true"));

    super::warp_tool_message_from_openai_tool_call(&call, "task", "req", None, false)
        .expect("normal turns still allow the permission system to handle risky commands");
}

#[test]
fn test_dsml_tool_call_content_is_hidden_when_structured_tool_calls_exist() {
    let content = Some("<｜｜DSML｜｜tool_calls>...</｜｜DSML｜｜tool_calls>".to_string());
    assert!(super::sanitize_local_visible_content(content).is_none());

    let content = Some("I will check the disk usage.".to_string());
    assert_eq!(
        super::sanitize_local_visible_content(content).as_deref(),
        Some("I will check the disk usage.")
    );
}

#[test]
fn test_dsml_tool_call_content_filters_protocol_and_keeps_visible_text() {
    let content = Some(
        "I need one more check.\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"run_shell_command\">\n<｜｜DSML｜｜parameter name=\"command\" string=\"true\">df -h</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>"
            .to_string(),
    );

    assert_eq!(
        super::sanitize_local_visible_content(content).as_deref(),
        Some("I need one more check.")
    );
}

#[test]
fn test_dsml_only_visible_content_is_hidden() {
    let content = Some("<｜｜DSML｜｜tool_calls>...</｜｜DSML｜｜tool_calls>".to_string());
    assert!(super::sanitize_local_visible_content(content).is_none());
}

#[test]
fn test_local_custom_repeated_tool_calls_are_forwarded() {
    let mut params = request_params_with_ask_user_question_enabled(false);
    params.input_task_id = Some(crate::ai::agent::task::TaskId::new(
        "test-repeated-tool-call".to_string(),
    ));
    let output = super::LocalModelOutput::ToolCalls {
        content: None,
        reasoning_content: None,
        tool_calls: vec![
            super::OpenAIToolCall {
                id: "call_1".to_string(),
                r#type: "function".to_string(),
                function: super::OpenAIToolCallFunction {
                    name: "run_shell_command".to_string(),
                    arguments: serde_json::json!({
                        "command": "df -h",
                        "is_read_only": true,
                        "is_risky": false
                    })
                    .to_string(),
                },
            },
            super::OpenAIToolCall {
                id: "call_2".to_string(),
                r#type: "function".to_string(),
                function: super::OpenAIToolCallFunction {
                    name: "run_shell_command".to_string(),
                    arguments: serde_json::json!({
                        "is_risky": false,
                        "command": "df -h",
                        "is_read_only": true
                    })
                    .to_string(),
                },
            },
        ],
    };

    let events = super::local_response_events(&params, output);
    let mut tool_call_count = 0;
    let mut stopped_message = false;
    for event in events.into_iter().flatten() {
        let Some(warp_multi_agent_api::response_event::Type::ClientActions(actions)) = event.r#type
        else {
            continue;
        };
        for action in actions.actions {
            let Some(warp_multi_agent_api::client_action::Action::AddMessagesToTask(add)) =
                action.action
            else {
                continue;
            };
            for message in add.messages {
                match message.message {
                    Some(warp_multi_agent_api::message::Message::ToolCall(_)) => {
                        tool_call_count += 1;
                    }
                    Some(warp_multi_agent_api::message::Message::AgentOutput(output)) => {
                        stopped_message |= output.text.contains("Repeated local tool call");
                    }
                    _ => {}
                }
            }
        }
    }

    assert_eq!(tool_call_count, 2);
    assert!(!stopped_message);
}
