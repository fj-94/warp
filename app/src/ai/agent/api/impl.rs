use std::{collections::HashMap, sync::Arc};

use crate::{ai::agent::redaction, terminal::model::session::SessionType};
use futures_util::{stream, StreamExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Value};
use warp_core::features::FeatureFlag;
use warp_multi_agent_api as api;

use crate::server::server_api::{AIApiError, ServerApi};

use super::{
    convert_to::convert_input, ConvertToAPITypeError, Event, RequestParams, ResponseStream,
};

pub async fn generate_multi_agent_output(
    server_api: Arc<ServerApi>,
    mut params: RequestParams,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> Result<ResponseStream, ConvertToAPITypeError> {
    if params.should_redact_secrets {
        redaction::redact_inputs(&mut params.input);
    }

    if let Some(local_endpoint) = local_custom_endpoint_for_model(&params) {
        return Ok(
            generate_local_custom_endpoint_output(params, local_endpoint, cancellation_rx).await,
        );
    }

    let supported_tools = params
        .supported_tools_override
        .take()
        .unwrap_or_else(|| get_supported_tools(&params));
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);
    let mut logging_metadata = HashMap::new();
    if let Some(metadata) = params.metadata {
        logging_metadata.insert(
            "is_autodetected_user_query".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_autodetected_user_query,
                )),
            },
        );
        logging_metadata.insert(
            "entrypoint".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    metadata.entrypoint.entrypoint(),
                )),
            },
        );
        logging_metadata.insert(
            "is_auto_resume_after_error".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_auto_resume_after_error,
                )),
            },
        );
    }

    let api_keys = api_keys_with_warp_credit_fallback_setting(
        params.api_keys,
        params.allow_use_of_warp_credits,
    );

    let request = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: params.tasks,
        }),
        input: Some(convert_input(params.input)?),
        settings: Some(api::request::Settings {
            model_config: Some(api::request::settings::ModelConfig {
                base: params.model.into(),
                cli_agent: params.cli_agent_model.into(),
                computer_use_agent: params.computer_use_model.into(),
                base_model_context_window_limit: if FeatureFlag::ConfigurableContextWindow
                    .is_enabled()
                {
                    params.context_window_limit.unwrap_or(0)
                } else {
                    0
                },
                ..Default::default()
            }),
            rules_enabled: params.is_memory_enabled,
            warp_drive_context_enabled: params.warp_drive_context_enabled,
            web_context_retrieval_enabled: true,
            supports_parallel_tool_calls: true,
            use_anthropic_text_editor_tools: false,
            planning_enabled: params.planning_enabled,
            supports_create_files: true,
            supported_tools: supported_tools.into_iter().map(Into::into).collect(),
            supports_long_running_commands: true,
            should_preserve_file_content_in_history: true,
            supports_todos_ui: true,
            supports_linked_code_blocks: FeatureFlag::LinkedCodeBlocks.is_enabled(),
            supports_started_child_task_message: true,
            supports_suggest_prompt: true,
            supports_read_image_files: FeatureFlag::ReadImageFiles.is_enabled(),
            supports_reasoning_message: true,
            api_keys,
            autonomy_level: params.autonomy_level.into(),
            isolation_level: params.isolation_level.into(),
            web_search_enabled: params.web_search_enabled,
            supported_cli_agent_tools: supported_cli_agent_tools
                .into_iter()
                .map(Into::into)
                .collect(),
            supports_v4a_file_diffs: FeatureFlag::V4AFileDiffs.is_enabled(),
            supports_summarization_via_message_replacement:
                FeatureFlag::SummarizationViaMessageReplacement.is_enabled(),
            supports_bundled_skills: FeatureFlag::BundledSkills.is_enabled(),
            supports_research_agent: params.research_agent_enabled,
            supports_orchestration_v2: FeatureFlag::OrchestrationV2.is_enabled(),
            custom_model_providers: params.custom_model_providers,
        }),
        metadata: Some(api::request::Metadata {
            logging: logging_metadata,
            conversation_id: params
                .conversation_token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            ambient_agent_task_id: params
                .ambient_agent_task_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            forked_from_conversation_id: if params.conversation_token.is_none() {
                // We only include this param on our initial request to the server
                // (when the forked conversation has not been assigned a new id yet).
                params
                    .forked_from_conversation_token
                    .map(|token| token.as_str().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            },
            parent_agent_id: params.parent_agent_id.unwrap_or_default(),
            agent_name: params.agent_name.unwrap_or_default(),
        }),
        existing_suggestions: params
            .existing_suggestions
            .map(|suggestions| suggestions.into()),
        mcp_context: params.mcp_context.map(Into::into),
    };

    let response_stream = server_api.generate_multi_agent_output(&request).await;
    match response_stream {
        Ok(stream) => {
            let output_stream = stream.take_until(cancellation_rx);
            Ok(Box::pin(output_stream))
        }
        Err(e) => {
            let (tx, rx) = async_channel::unbounded();
            let _ = tx.send(Err(e)).await;
            Ok(Box::pin(rx))
        }
    }
}

#[derive(Debug, Clone)]
struct LocalCustomEndpoint {
    base_url: String,
    api_key: String,
    model: String,
}

fn local_custom_endpoint_for_model(params: &RequestParams) -> Option<LocalCustomEndpoint> {
    let selected_model_ids = [
        params.model.as_str(),
        params.cli_agent_model.as_str(),
        params.computer_use_model.as_str(),
    ];

    let endpoint = params
        .custom_model_providers
        .as_ref()?
        .providers
        .iter()
        .find_map(|provider| {
            provider.models.iter().find_map(|model| {
                selected_model_ids
                    .iter()
                    .any(|selected_model_id| {
                        model.config_key == *selected_model_id || model.slug == *selected_model_id
                    })
                    .then(|| LocalCustomEndpoint {
                        base_url: provider.base_url.clone(),
                        api_key: provider.api_key.clone(),
                        model: model.slug.clone(),
                    })
            })
        })
        .filter(|endpoint| {
            !endpoint.base_url.trim().is_empty()
                && !endpoint.api_key.is_empty()
                && !endpoint.model.trim().is_empty()
        })?;

    log::info!(
        "Using local custom inference endpoint for model {} at {}",
        endpoint.model,
        endpoint.base_url
    );

    Some(endpoint)
}

async fn generate_local_custom_endpoint_output(
    params: RequestParams,
    endpoint: LocalCustomEndpoint,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> ResponseStream {
    let events = match call_openai_compatible_chat_completion(&params, &endpoint).await {
        Ok(output) => local_response_events(&params, output),
        Err(err) => vec![Err(Arc::new(err))],
    };

    Box::pin(stream::iter(events).take_until(cancellation_rx))
}

#[derive(serde::Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatCompletionMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAITool>,
    stream: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ChatCompletionMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct OpenAITool {
    r#type: &'static str,
    function: OpenAIFunction,
}

#[derive(Clone, Debug, serde::Serialize)]
struct OpenAIFunction {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[derive(serde::Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(serde::Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionResponseMessage,
}

#[derive(serde::Deserialize)]
struct ChatCompletionResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct OpenAIToolCall {
    id: String,
    r#type: String,
    function: OpenAIToolCallFunction,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct OpenAIToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Clone, Debug, Default)]
struct LocalConversationState {
    messages: Vec<ChatCompletionMessage>,
    pending_tool_calls: HashMap<String, OpenAIToolCall>,
    active_command_id: Option<String>,
    current_user_goal: Option<String>,
}

static LOCAL_CONVERSATIONS: Lazy<Mutex<HashMap<String, LocalConversationState>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
enum LocalModelOutput {
    Text {
        content: String,
        reasoning_content: Option<String>,
    },
    ToolCalls {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<OpenAIToolCall>,
    },
}

async fn call_openai_compatible_chat_completion(
    params: &RequestParams,
    endpoint: &LocalCustomEndpoint,
) -> Result<LocalModelOutput, AIApiError> {
    let url = chat_completions_url(&endpoint.base_url);
    let messages = local_openai_messages(params);
    let tools = local_openai_tools(params);
    log::info!(
        "Local custom request: input_task_id={:?}, task_ids={:?}, active_command_id={:?}, tool_count={}, message_count={}",
        params.input_task_id.as_ref().map(ToString::to_string),
        params
            .tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        local_active_command_id(params),
        tools.len(),
        messages.len()
    );
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(&endpoint.api_key)
        .json(&ChatCompletionRequest {
            model: endpoint.model.clone(),
            messages,
            tools,
            stream: false,
        })
        .send()
        .await
        .map_err(|err| AIApiError::Other(err.into()))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| AIApiError::Other(err.into()))?;

    if !status.is_success() {
        return Err(AIApiError::Other(anyhow::anyhow!(
            "Custom endpoint returned {status}: {body}"
        )));
    }

    let parsed: ChatCompletionResponse =
        serde_json::from_str(&body).map_err(|err| AIApiError::Other(err.into()))?;
    let message = parsed
        .choices
        .into_iter()
        .map(|choice| choice.message)
        .next()
        .ok_or_else(|| AIApiError::Other(anyhow::anyhow!("Custom endpoint returned no choices")))?;

    if let Some(tool_calls) = message.tool_calls.filter(|calls| !calls.is_empty()) {
        return Ok(LocalModelOutput::ToolCalls {
            content: sanitize_local_visible_content(message.content),
            reasoning_content: message.reasoning_content,
            tool_calls,
        });
    }

    if let Some(content) = message.content.as_deref() {
        if let Some(tool_calls) = parse_dsml_tool_calls(content) {
            log::info!(
                "Local custom parsed {} DSML text tool call(s) from model content",
                tool_calls.len()
            );
            return Ok(LocalModelOutput::ToolCalls {
                content: None,
                reasoning_content: message.reasoning_content,
                tool_calls,
            });
        }
    }

    sanitize_local_visible_content(message.content)
        .map(|content| LocalModelOutput::Text {
            content,
            reasoning_content: message.reasoning_content,
        })
        .ok_or_else(|| AIApiError::Other(anyhow::anyhow!("Custom endpoint returned no text")))
}

fn chat_completions_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

fn parse_dsml_tool_calls(content: &str) -> Option<Vec<OpenAIToolCall>> {
    if !content.contains("tool_calls") || !content.contains("invoke name=") {
        return None;
    }

    let mut tool_calls = vec![];
    // Avoid byte slicing: DSML content contains multi-byte characters like `｜`.
    // We only use safe string splitting and `find` on owned substrings.
    for segment in content.split("invoke name=").skip(1) {
        let name = parse_dsml_quoted_value(segment)?;
        let invoke_end_tag = "</｜DSML｜invoke>";
        let invoke_body = segment.splitn(2, invoke_end_tag).next().unwrap_or(segment);
        let args = parse_dsml_parameters(invoke_body);
        tool_calls.push(OpenAIToolCall {
            id: format!("local_dsml_{}", uuid::Uuid::new_v4()),
            r#type: "function".to_string(),
            function: OpenAIToolCallFunction {
                name,
                arguments: serde_json::to_string(&Value::Object(args)).ok()?,
            },
        });
    }

    (!tool_calls.is_empty()).then_some(tool_calls)
}

fn parse_dsml_parameters(body: &str) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    let mut search_from = 0;
    while let Some(parameter_offset) = body[search_from..].find("parameter name=") {
        let parameter_start = search_from + parameter_offset;
        let after_name = parameter_start + "parameter name=".len();
        let Some(name) = parse_dsml_quoted_value(&body[after_name..]) else {
            break;
        };
        let Some(open_end) = body[after_name..].find(">") else {
            break;
        };
        let value_start = after_name + open_end + 1;
        // Be charset-agnostic: DSML producers may use different "pipe" characters or tags.
        // Take the parameter value as text up to the next '<' (start of a closing tag).
        let value_end = body[value_start..]
            .find('<')
            .map(|off| value_start + off)
            .unwrap_or(body.len());
        let raw_value = body[value_start..value_end].trim();
        args.insert(name, dsml_value(raw_value));
        search_from = value_end;
    }
    args
}

fn parse_dsml_quoted_value(input: &str) -> Option<String> {
    let start = input.find('"')? + 1;
    let end = input[start..].find('"')?;
    Some(input[start..start + end].to_string())
}

fn dsml_value(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(value.to_string()),
    }
}

fn local_prompt_from_inputs(params: &RequestParams, active_command_id: Option<&str>) -> String {
    let mut parts = vec![];
    for input in &params.input {
        match input {
            crate::ai::agent::AIAgentInput::UserQuery {
                query,
                running_command,
                ..
            } => {
                if let Some(rc) = running_command {
                    // In remote-control / CLI subagent contexts, the model must see the current
                    // terminal snapshot; otherwise it will frequently answer based on stale context.
                    parts.push(format!(
                        "REMOTE TERMINAL SNAPSHOT (command_id={}):\ncommand: {}\n\noutput:\n{}",
                        rc.block_id.as_str(),
                        rc.command,
                        rc.grid_contents
                    ));
                } else if let Some(command_id) = active_command_id {
                    parts.push(format!(
                        "REMOTE TERMINAL ACTIVE (command_id={command_id}). If you need current state, call read_shell_command_output first."
                    ));
                }
                parts.push(format!("USER QUESTION:\n{query}"));
            }
            crate::ai::agent::AIAgentInput::SummarizeConversation { prompt, .. } => {
                parts.push(
                    prompt
                        .clone()
                        .unwrap_or_else(|| "Summarize this conversation.".to_string()),
                );
            }
            crate::ai::agent::AIAgentInput::CreateNewProject { query, .. } => {
                parts.push(query.clone());
            }
            crate::ai::agent::AIAgentInput::CloneRepository { clone_repo_url, .. } => {
                parts.push(format!(
                    "Clone or discuss this repository: {}",
                    clone_repo_url.clone().into_url()
                ));
            }
            other => {
                parts.push(format!("{other:?}"));
            }
        }
    }

    parts.join("\n\n")
}

fn local_user_query_text(params: &RequestParams) -> Option<String> {
    let queries = params
        .input
        .iter()
        .filter_map(|input| {
            if let crate::ai::agent::AIAgentInput::UserQuery { query, .. } = input {
                Some(query.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    (!queries.is_empty()).then(|| queries.join("\n"))
}

fn local_openai_messages(params: &RequestParams) -> Vec<ChatCompletionMessage> {
    let key = local_conversation_key(params);
    let mut state = LOCAL_CONVERSATIONS.lock();
    let conversation = state.entry(key).or_default();
    if let Some(command_id) = observed_active_running_command_id(params) {
        conversation.active_command_id = Some(command_id);
    }
    let active_command_id = conversation.active_command_id.clone();
    // Only add the tool-result continuation instruction when the request contains action results
    // but does not contain a new user query. User turns should be modeled as new user messages.
    let has_user_query = params
        .input
        .iter()
        .any(|input| matches!(input, crate::ai::agent::AIAgentInput::UserQuery { .. }));

    if conversation.messages.is_empty() {
        conversation.messages.push(ChatCompletionMessage {
            role: "system".to_string(),
            content: Some("You are Warp Agent running locally with access to Warp tools. Use tools when you need terminal output, shell commands, files, search, or long-running command control. For remote-control or long-running terminal sessions, call read_shell_command_output to inspect the screen, write_to_long_running_shell_command to send input, and transfer_shell_command_control_to_user when the user should take control. Reply with final text only after tools are done.".to_string()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }

    if has_user_query {
        conversation.current_user_goal = local_user_query_text(params);
    }

    let mut appended_action_result = false;
    for input in &params.input {
        if let crate::ai::agent::AIAgentInput::ActionResult { result, .. } = input {
            let tool_call_id = result.id.to_string();
            let tool_result = truncate_tool_result(format!(
                "{}",
                crate::ai::agent::MarkdownActionResult(&result.result)
            ));
            conversation.messages.push(ChatCompletionMessage {
                role: "tool".to_string(),
                content: Some(tool_result),
                reasoning_content: None,
                tool_call_id: Some(tool_call_id.clone()),
                tool_calls: None,
            });
            conversation.pending_tool_calls.remove(&tool_call_id);
            appended_action_result = true;
        }
    }

    conversation.messages.retain(|message| {
        !is_local_summary_instruction(message) && !is_dsml_assistant_text(message)
    });

    if !conversation.pending_tool_calls.is_empty() {
        let pending_tool_call_ids: Vec<_> =
            conversation.pending_tool_calls.keys().cloned().collect();
        for tool_call_id in pending_tool_call_ids {
            conversation.messages.push(ChatCompletionMessage {
                role: "tool".to_string(),
                content: Some(
                    "The tool call did not produce a client result before the next request. Treat it as interrupted and continue safely.".to_string(),
                ),
                reasoning_content: None,
                tool_call_id: Some(tool_call_id.clone()),
                tool_calls: None,
            });
            conversation.pending_tool_calls.remove(&tool_call_id);
        }
    }

    if has_user_query || !appended_action_result {
        let prompt = local_prompt_from_inputs(params, active_command_id.as_deref());
        conversation.messages.push(ChatCompletionMessage {
            role: "user".to_string(),
            content: Some(prompt),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }

    if conversation.messages.len() > 40 {
        let system = conversation.messages.first().cloned();
        let keep_from = conversation.messages.len().saturating_sub(39);
        let mut trimmed = system.into_iter().collect::<Vec<_>>();
        trimmed.extend(conversation.messages.drain(keep_from..));
        conversation.messages = trimmed;
    }

    let mut sanitized_messages = sanitize_openai_tool_messages(conversation.messages.clone());
    if appended_action_result && !has_user_query {
        sanitized_messages.push(local_continue_after_tool_result_instruction_message(
            conversation.current_user_goal.as_deref(),
        ));
    }
    conversation.messages = sanitized_messages.clone();
    sanitized_messages
}

fn local_response_events(params: &RequestParams, output: LocalModelOutput) -> Vec<Event> {
    let mut envelope = LocalEventEnvelope::new(params);
    let events = match output {
        LocalModelOutput::Text {
            content,
            reasoning_content,
        } => {
            remember_assistant_text(params, &content, reasoning_content.as_deref());
            envelope.push_agent_output(content);
            envelope.push_finished();
            envelope.events
        }
        LocalModelOutput::ToolCalls {
            content,
            reasoning_content,
            tool_calls,
        } => {
            let default_command_id = local_active_command_id(params);
            log::info!(
                "Local custom model returned {} tool call(s): {:?}; default_command_id={:?}",
                tool_calls.len(),
                tool_calls
                    .iter()
                    .map(|call| format!("{}:{}", call.id, call.function.name))
                    .collect::<Vec<_>>(),
                default_command_id
            );
            let mut valid_tool_calls = vec![];
            let mut invalid_tool_messages = vec![];
            let mut local_tool_outputs = vec![];
            for tool_call in tool_calls {
                if let Some(output) = local_output_for_non_executed_tool_call(&tool_call) {
                    local_tool_outputs.push(output);
                    continue;
                }

                match warp_tool_message_from_openai_tool_call(
                    &tool_call,
                    &envelope.task_id,
                    &envelope.request_id,
                    default_command_id.as_deref(),
                    false,
                ) {
                    Ok(message) => valid_tool_calls.push((tool_call, message)),
                    Err(error) => invalid_tool_messages.push(format!(
                        "Unsupported local tool call `{}`: {error}",
                        tool_call.function.name
                    )),
                }
            }

            if let Some(content) = content
                .as_ref()
                .filter(|content| !content.trim().is_empty())
            {
                envelope.push_agent_output(content.clone());
            }

            if !valid_tool_calls.is_empty() {
                let remembered_calls: Vec<_> = valid_tool_calls
                    .iter()
                    .map(|(tool_call, _)| tool_call.clone())
                    .collect();
                remember_assistant_tool_calls(
                    params,
                    content.as_deref(),
                    reasoning_content.as_deref(),
                    &remembered_calls,
                );
            }

            for (_, message) in valid_tool_calls {
                envelope.push_message(message);
            }

            for output in local_tool_outputs {
                envelope.push_agent_output(output);
            }

            for message in invalid_tool_messages {
                envelope.push_agent_output(message);
            }

            if !envelope.has_tool_call && !envelope.has_agent_output {
                envelope.push_agent_output(
                    "The model requested tools, but none of the tool calls could be handled locally."
                        .to_string(),
                );
            }
            envelope.push_finished();
            envelope.events
        }
    };

    events
}

fn local_output_for_non_executed_tool_call(tool_call: &OpenAIToolCall) -> Option<String> {
    if tool_call.function.name != "transfer_shell_command_control_to_user" {
        return None;
    }

    let reason = serde_json::from_str::<Value>(&tool_call.function.arguments)
        .ok()
        .and_then(|args| string_arg_optional(&args, "reason"))
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "Local custom model finished responding.".to_string());
    Some(reason)
}

fn sanitize_openai_tool_messages(
    messages: Vec<ChatCompletionMessage>,
) -> Vec<ChatCompletionMessage> {
    let mut sanitized = Vec::with_capacity(messages.len());
    let mut pending_tool_call_ids: Vec<String> = vec![];

    for mut message in messages {
        if pending_tool_call_ids.is_empty() {
            if message.role == "tool" {
                continue;
            }
        } else if message.role != "tool" {
            append_interrupted_tool_results(
                &mut sanitized,
                std::mem::take(&mut pending_tool_call_ids),
            );
        }

        if message.role == "tool" {
            let Some(tool_call_id) = message.tool_call_id.clone() else {
                continue;
            };
            if let Some(index) = pending_tool_call_ids
                .iter()
                .position(|pending_id| pending_id == &tool_call_id)
            {
                pending_tool_call_ids.remove(index);
                sanitized.push(message);
            }
            continue;
        }

        if message.role == "assistant" {
            let tool_calls = message
                .tool_calls
                .take()
                .filter(|tool_calls| !tool_calls.is_empty());
            pending_tool_call_ids = tool_calls
                .as_ref()
                .map(|tool_calls| {
                    tool_calls
                        .iter()
                        .map(|tool_call| tool_call.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            message.tool_calls = tool_calls;
        }

        sanitized.push(message);
    }

    if !pending_tool_call_ids.is_empty() {
        append_interrupted_tool_results(&mut sanitized, pending_tool_call_ids);
    }

    sanitized
}

fn append_interrupted_tool_results(
    messages: &mut Vec<ChatCompletionMessage>,
    pending_tool_call_ids: Vec<String>,
) {
    for tool_call_id in pending_tool_call_ids {
        messages.push(ChatCompletionMessage {
            role: "tool".to_string(),
            content: Some(
                "The Warp client did not return a result for this tool call before the next request. Treat the tool call as interrupted and continue safely.".to_string(),
            ),
            reasoning_content: None,
            tool_call_id: Some(tool_call_id),
            tool_calls: None,
        });
    }
}

fn local_continue_after_tool_result_instruction_message(
    goal: Option<&str>,
) -> ChatCompletionMessage {
    let goal_suffix = goal
        .filter(|goal| !goal.trim().is_empty())
        .map(|goal| format!(" Current user goal: {goal}"))
        .unwrap_or_default();
    ChatCompletionMessage {
        role: "system".to_string(),
        content: Some(
            format!(
                "You received a tool result.{goal_suffix} Continue using tools until you have enough evidence to complete the user's goal. If the result is enough, provide the final answer now. Do not repeat a tool call that produced no new information."
            ),
        ),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn is_local_summary_instruction(message: &ChatCompletionMessage) -> bool {
    message.role == "system"
        && message.content.as_deref().is_some_and(|content| {
            content.starts_with("You have received the tool result.")
                || content.starts_with("You received a terminal snapshot after writing")
                || content.starts_with("You received a successful read-only shell command result.")
                || content.starts_with("You received a successful tool result.")
                || content.starts_with("You received a tool result.")
        })
}

fn is_dsml_assistant_text(message: &ChatCompletionMessage) -> bool {
    message.role == "assistant"
        && message.tool_calls.is_none()
        && message
            .content
            .as_deref()
            .is_some_and(|content| content.contains("tool_calls") && content.contains("DSML"))
}

fn sanitize_local_visible_content(content: Option<String>) -> Option<String> {
    let content = content?;
    if !looks_like_dsml_tool_text(&content) {
        return (!content.trim().is_empty()).then_some(content);
    }

    let visible = content
        .lines()
        .filter(|line| !looks_like_dsml_tool_line(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (!visible.is_empty()).then_some(visible)
}

fn looks_like_dsml_tool_text(content: &str) -> bool {
    content.contains("DSML") && (content.contains("tool_calls") || content.contains("invoke name="))
}

fn looks_like_dsml_tool_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("DSML")
        || trimmed.starts_with("<|")
        || trimmed.starts_with("</|")
        || trimmed.contains("invoke name=")
        || trimmed.contains("parameter name=")
}

struct LocalEventEnvelope {
    events: Vec<Event>,
    task_id: String,
    request_id: String,
    has_tool_call: bool,
    has_agent_output: bool,
}

impl LocalEventEnvelope {
    fn new(params: &RequestParams) -> Self {
        let conversation_id = params
            .conversation_token
            .as_ref()
            .map(|token| token.as_str().to_string())
            .unwrap_or_default();
        let request_id = uuid::Uuid::new_v4().to_string();
        let should_create_root_task = params.tasks.is_empty() && params.input_task_id.is_none();
        let mut target_task = target_task_for_local_response(params).unwrap_or_else(|| api::Task {
            id: params
                .input_task_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            ..Default::default()
        });
        if target_task.id.is_empty() {
            target_task.id = uuid::Uuid::new_v4().to_string();
        }
        target_task.messages.clear();
        let task_id = target_task.id.clone();
        log::info!(
            "Local custom response envelope: target_task_id={}, should_create_task={}, input_task_id={:?}",
            task_id,
            should_create_root_task,
            params.input_task_id.as_ref().map(ToString::to_string)
        );

        let mut events = vec![Ok(api::ResponseEvent {
            r#type: Some(api::response_event::Type::Init(
                api::response_event::StreamInit {
                    conversation_id,
                    request_id: request_id.clone(),
                    run_id: String::new(),
                },
            )),
        })];
        if should_create_root_task {
            events.push(Ok(api::ResponseEvent {
                r#type: Some(api::response_event::Type::ClientActions(
                    api::response_event::ClientActions {
                        actions: vec![api::ClientAction {
                            action: Some(api::client_action::Action::CreateTask(
                                api::client_action::CreateTask {
                                    task: Some(target_task),
                                },
                            )),
                        }],
                    },
                )),
            }));
        }

        Self {
            events,
            task_id,
            request_id,
            has_tool_call: false,
            has_agent_output: false,
        }
    }

    fn push_agent_output(&mut self, text: String) {
        self.has_agent_output = true;
        self.push_message(api::Message {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: self.task_id.clone(),
            request_id: self.request_id.clone(),
            timestamp: Some(prost_types::Timestamp {
                seconds: chrono::Utc::now().timestamp(),
                nanos: 0,
            }),
            message: Some(api::message::Message::AgentOutput(
                api::message::AgentOutput { text },
            )),
            ..Default::default()
        });
    }

    fn push_message(&mut self, message: api::Message) {
        if matches!(message.message, Some(api::message::Message::ToolCall(_))) {
            self.has_tool_call = true;
            if let Some(api::message::Message::ToolCall(tool_call)) = message.message.as_ref() {
                log::info!(
                    "Local custom AddMessagesToTask tool call: task_id={}, message_task_id={}, request_id={}, tool_call_id={}, tool={}",
                    self.task_id,
                    message.task_id,
                    message.request_id,
                    tool_call.tool_call_id,
                    local_tool_name(tool_call)
                );
            }
        }
        self.events.push(Ok(api::ResponseEvent {
            r#type: Some(api::response_event::Type::ClientActions(
                api::response_event::ClientActions {
                    actions: vec![api::ClientAction {
                        action: Some(api::client_action::Action::AddMessagesToTask(
                            api::client_action::AddMessagesToTask {
                                task_id: self.task_id.clone(),
                                messages: vec![message],
                            },
                        )),
                    }],
                },
            )),
        }));
    }

    fn push_finished(&mut self) {
        self.events.push(Ok(api::ResponseEvent {
            r#type: Some(api::response_event::Type::Finished(
                api::response_event::StreamFinished {
                    reason: Some(api::response_event::stream_finished::Reason::Done(
                        api::response_event::stream_finished::Done {},
                    )),
                    token_usage: vec![],
                    should_refresh_model_config: false,
                    request_cost: None,
                    conversation_usage_metadata: Some(
                        api::response_event::stream_finished::ConversationUsageMetadata {
                            context_window_usage: 0.0,
                            summarized: false,
                            credits_spent: 0.0,
                            #[allow(deprecated)]
                            token_usage: vec![],
                            tool_usage_metadata: None,
                            warp_token_usage: HashMap::new(),
                            byok_token_usage: HashMap::new(),
                        },
                    ),
                },
            )),
        }));
    }
}

fn target_task_for_local_response(params: &RequestParams) -> Option<api::Task> {
    if let Some(input_task_id) = &params.input_task_id {
        if let Some(task) = params
            .tasks
            .iter()
            .find(|task| task.id == input_task_id.to_string())
        {
            return Some(task.clone());
        }

        if params.tasks.is_empty() {
            return Some(api::Task {
                id: input_task_id.to_string(),
                ..Default::default()
            });
        }
    }

    params
        .tasks
        .iter()
        .find(|task| {
            task.dependencies
                .as_ref()
                .is_some_and(|dependencies| !dependencies.parent_task_id.is_empty())
        })
        .or_else(|| params.tasks.last())
        .cloned()
}

fn local_conversation_key(params: &RequestParams) -> String {
    params
        .conversation_token
        .as_ref()
        .map(|token| token.as_str().to_string())
        .or_else(|| params.input_task_id.as_ref().map(ToString::to_string))
        .or_else(|| params.tasks.first().map(|task| task.id.clone()))
        .unwrap_or_else(|| "local-custom-default".to_string())
}

fn observed_active_running_command_id(params: &RequestParams) -> Option<String> {
    params.input.iter().find_map(|input| {
        if let crate::ai::agent::AIAgentInput::UserQuery {
            running_command: Some(running_command),
            ..
        } = input
        {
            Some(running_command.block_id.as_str().to_string())
        } else {
            None
        }
    })
}

fn local_active_command_id(params: &RequestParams) -> Option<String> {
    observed_active_running_command_id(params).or_else(|| {
        let key = local_conversation_key(params);
        LOCAL_CONVERSATIONS
            .lock()
            .get(&key)
            .and_then(|conversation| conversation.active_command_id.clone())
    })
}

fn remember_assistant_text(params: &RequestParams, text: &str, reasoning_content: Option<&str>) {
    let key = local_conversation_key(params);
    let mut state = LOCAL_CONVERSATIONS.lock();
    let conversation = state.entry(key).or_default();
    conversation.messages.push(ChatCompletionMessage {
        role: "assistant".to_string(),
        content: Some(text.to_string()),
        reasoning_content: reasoning_content.map(str::to_string),
        tool_call_id: None,
        tool_calls: None,
    });
}

fn remember_assistant_tool_calls(
    params: &RequestParams,
    content: Option<&str>,
    reasoning_content: Option<&str>,
    tool_calls: &[OpenAIToolCall],
) {
    let key = local_conversation_key(params);
    let mut state = LOCAL_CONVERSATIONS.lock();
    let conversation = state.entry(key).or_default();
    conversation.messages.push(ChatCompletionMessage {
        role: "assistant".to_string(),
        content: content.map(str::to_string),
        reasoning_content: reasoning_content.map(str::to_string),
        tool_call_id: None,
        tool_calls: Some(tool_calls.to_vec()),
    });
    for tool_call in tool_calls {
        conversation
            .pending_tool_calls
            .insert(tool_call.id.clone(), tool_call.clone());
    }
}

fn local_openai_tools(params: &RequestParams) -> Vec<OpenAITool> {
    let supported_tools = params
        .supported_tools_override
        .clone()
        .unwrap_or_else(|| get_supported_tools(params));
    let supported_cli_agent_tools = get_supported_cli_agent_tools(params);
    let mut tools = vec![];

    if supported_tools.contains(&api::ToolType::RunShellCommand) {
        tools.push(run_shell_command_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::WriteToLongRunningShellCommand) {
        tools.push(write_to_long_running_shell_command_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::ReadShellCommandOutput) {
        tools.push(read_shell_command_output_tool_schema());
    }
    if supported_cli_agent_tools.contains(&api::ToolType::TransferShellCommandControlToUser) {
        tools.push(transfer_shell_command_control_to_user_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::ReadFiles) {
        tools.push(read_files_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::Grep) {
        tools.push(grep_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::FileGlobV2) {
        tools.push(file_glob_v2_tool_schema());
    }
    if supported_tools.contains(&api::ToolType::SearchCodebase) {
        tools.push(search_codebase_tool_schema());
    }

    tools
}

fn run_shell_command_tool_schema() -> OpenAITool {
    tool_schema(
        "run_shell_command",
        "Run a shell command and return its output.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "is_read_only": {"type": "boolean"},
                "uses_pager": {"type": "boolean"},
                "is_risky": {"type": "boolean"},
                "wait_until_complete": {"type": "boolean"}
            },
            "required": ["command"]
        }),
    )
}

fn read_shell_command_output_tool_schema() -> OpenAITool {
    tool_schema(
        "read_shell_command_output",
        "Read output from the active or specified long-running shell command.",
        json!({
            "type": "object",
            "properties": {
                "command_id": {"type": "string"},
                "delay_seconds": {"type": "integer", "minimum": 0}
            }
        }),
    )
}

fn write_to_long_running_shell_command_tool_schema() -> OpenAITool {
    tool_schema(
        "write_to_long_running_shell_command",
        "Write input to the active long-running shell command.",
        json!({
            "type": "object",
            "properties": {
                "command_id": {"type": "string"},
                "input": {"type": "string"},
                "mode": {"type": "string", "enum": ["raw", "line", "block"]}
            },
            "required": ["input"]
        }),
    )
}

fn transfer_shell_command_control_to_user_tool_schema() -> OpenAITool {
    tool_schema(
        "transfer_shell_command_control_to_user",
        "Transfer control of the active long-running command back to the user.",
        json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string"}
            },
            "required": ["reason"]
        }),
    )
}

fn read_files_tool_schema() -> OpenAITool {
    tool_schema(
        "read_files",
        "Read one or more files.",
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "start": {"type": "integer"},
                            "end": {"type": "integer"}
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["files"]
        }),
    )
}

fn grep_tool_schema() -> OpenAITool {
    tool_schema(
        "grep",
        "Search text or regex patterns in files.",
        json!({
            "type": "object",
            "properties": {
                "queries": {"type": "array", "items": {"type": "string"}},
                "path": {"type": "string"}
            },
            "required": ["queries"]
        }),
    )
}

fn file_glob_v2_tool_schema() -> OpenAITool {
    tool_schema(
        "file_glob_v2",
        "Find files by name patterns.",
        json!({
            "type": "object",
            "properties": {
                "patterns": {"type": "array", "items": {"type": "string"}},
                "search_dir": {"type": "string"},
                "max_matches": {"type": "integer"},
                "max_depth": {"type": "integer"},
                "min_depth": {"type": "integer"}
            },
            "required": ["patterns"]
        }),
    )
}

fn search_codebase_tool_schema() -> OpenAITool {
    tool_schema(
        "search_codebase",
        "Search the indexed codebase semantically.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "path_filters": {"type": "array", "items": {"type": "string"}},
                "codebase_path": {"type": "string"}
            },
            "required": ["query"]
        }),
    )
}

fn tool_schema(name: &'static str, description: &'static str, parameters: Value) -> OpenAITool {
    OpenAITool {
        r#type: "function",
        function: OpenAIFunction {
            name,
            description,
            parameters,
        },
    }
}

fn warp_tool_message_from_openai_tool_call(
    tool_call: &OpenAIToolCall,
    task_id: &str,
    request_id: &str,
    default_command_id: Option<&str>,
    enforce_read_only_shell_command: bool,
) -> Result<api::Message, String> {
    let args: Value = serde_json::from_str(&tool_call.function.arguments)
        .map_err(|err| format!("invalid JSON arguments: {err}"))?;
    let tool = match tool_call.function.name.as_str() {
        "run_shell_command" => {
            let command = string_arg(&args, "command")?;
            if enforce_read_only_shell_command
                && (bool_arg(&args, "is_risky", false) || !bool_arg(&args, "is_read_only", true))
            {
                return Err(
                    "run_shell_command follow-ups must be marked is_read_only=true and is_risky=false"
                        .to_string(),
                );
            }
            if let Some(command_id) = default_command_id {
                log::info!(
                    "Local custom remapping run_shell_command to write_to_long_running_shell_command: tool_call_id={}, command_id={}, command={}",
                    tool_call.id,
                    command_id,
                    command
                );
                api::message::tool_call::Tool::WriteToLongRunningShellCommand(
                    api::message::tool_call::WriteToLongRunningShellCommand {
                        input: command.into_bytes(),
                        mode: Some(write_mode(Some("line"))),
                        command_id: command_id.to_string(),
                    },
                )
            } else {
                api::message::tool_call::Tool::RunShellCommand(
                    api::message::tool_call::RunShellCommand {
                        command,
                is_read_only: bool_arg(&args, "is_read_only", true),
                uses_pager: bool_arg(&args, "uses_pager", false),
                is_risky: bool_arg(&args, "is_risky", false),
                risk_category: if bool_arg(&args, "is_risky", false) {
                    api::RiskCategory::Risky as i32
                } else if bool_arg(&args, "is_read_only", true) {
                    api::RiskCategory::ReadOnly as i32
                } else {
                    api::RiskCategory::Unspecified as i32
                },
                wait_until_complete_value: Some(
                    api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                        bool_arg(&args, "wait_until_complete", true),
                    ),
                ),
                citations: vec![],
                    },
                )
            }
        }
        "write_to_long_running_shell_command" => {
            // Some models (and DSML text tool calls) use `command`/`text` instead of `input`.
            let input = string_arg_optional(&args, "input")
                .or_else(|| string_arg_optional(&args, "command"))
                .or_else(|| string_arg_optional(&args, "text"))
                .ok_or_else(|| "missing string argument `input`".to_string())?;
            let command_id = command_id_arg_or_default(&args, default_command_id);
            // Default to `line` so the input actually executes. `raw` would only type into the PTY.
            // We keep supporting explicit `mode` overrides (raw/line/block).
            let mode_str = string_arg_optional(&args, "mode").unwrap_or_else(|| "line".to_string());
            log::info!(
                "Local custom mapping write_to_long_running_shell_command: tool_call_id={}, command_id={}",
                tool_call.id,
                command_id
            );
            api::message::tool_call::Tool::WriteToLongRunningShellCommand(
                api::message::tool_call::WriteToLongRunningShellCommand {
                    input: input.into_bytes(),
                    mode: Some(write_mode(Some(mode_str.as_str()))),
                    command_id,
                },
            )
        }
        "read_shell_command_output" => api::message::tool_call::Tool::ReadShellCommandOutput(
            api::message::tool_call::ReadShellCommandOutput {
                command_id: command_id_arg_or_default(&args, default_command_id),
                delay: int_arg(&args, "delay_seconds").map(|seconds| {
                    api::message::tool_call::read_shell_command_output::Delay::Duration(
                        prost_types::Duration { seconds, nanos: 0 },
                    )
                }),
            },
        ),
        "transfer_shell_command_control_to_user" => {
            return Err("transfer_shell_command_control_to_user is handled locally".to_string());
        }
        "read_files" => {
            api::message::tool_call::Tool::ReadFiles(api::message::tool_call::ReadFiles {
                files: array_arg(&args, "files")
                    .into_iter()
                    .filter_map(read_file_arg)
                    .collect(),
            })
        }
        "grep" => api::message::tool_call::Tool::Grep(api::message::tool_call::Grep {
            queries: string_array_arg(&args, "queries"),
            path: string_arg_optional(&args, "path").unwrap_or_default(),
        }),
        "file_glob_v2" => {
            api::message::tool_call::Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
                patterns: string_array_arg(&args, "patterns"),
                search_dir: string_arg_optional(&args, "search_dir").unwrap_or_default(),
                max_matches: int_arg(&args, "max_matches").unwrap_or_default() as i32,
                max_depth: int_arg(&args, "max_depth").unwrap_or_default() as i32,
                min_depth: int_arg(&args, "min_depth").unwrap_or_default() as i32,
            })
        }
        "search_codebase" => {
            api::message::tool_call::Tool::SearchCodebase(api::message::tool_call::SearchCodebase {
                query: string_arg(&args, "query")?,
                path_filters: string_array_arg(&args, "path_filters"),
                codebase_path: string_arg_optional(&args, "codebase_path").unwrap_or_default(),
            })
        }
        other => return Err(format!("unsupported tool `{other}`")),
    };

    Ok(api::Message {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: Some(prost_types::Timestamp {
            seconds: chrono::Utc::now().timestamp(),
            nanos: 0,
        }),
        message: Some(api::message::Message::ToolCall(api::message::ToolCall {
            tool_call_id: tool_call.id.clone(),
            tool: Some(tool),
        })),
        ..Default::default()
    })
}

fn write_mode(
    mode: Option<&str>,
) -> api::message::tool_call::write_to_long_running_shell_command::Mode {
    use api::message::tool_call::write_to_long_running_shell_command::mode::Mode;
    let normalized_mode = mode.map(|mode| mode.trim().to_ascii_lowercase());
    let mode = match normalized_mode.as_deref() {
        Some("line" | "enter" | "submit" | "execute" | "command") => Mode::Line(()),
        Some("block" | "paste") => Mode::Block(()),
        Some("raw") => Mode::Raw(()),
        _ => Mode::Line(()),
    };
    api::message::tool_call::write_to_long_running_shell_command::Mode { mode: Some(mode) }
}

fn read_file_arg(value: &Value) -> Option<api::message::tool_call::read_files::File> {
    let name = value.get("name")?.as_str()?.to_string();
    let start = value.get("start").and_then(Value::as_u64);
    let end = value.get("end").and_then(Value::as_u64);
    let line_ranges = match (start, end) {
        (Some(start), Some(end)) => vec![api::FileContentLineRange {
            start: start as u32,
            end: end as u32,
        }],
        _ => vec![],
    };
    Some(api::message::tool_call::read_files::File { name, line_ranges })
}

fn string_arg(args: &Value, key: &str) -> Result<String, String> {
    string_arg_optional(args, key).ok_or_else(|| format!("missing string argument `{key}`"))
}

fn string_arg_optional(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn command_id_arg_or_default(args: &Value, default_command_id: Option<&str>) -> String {
    // DSML / some providers may use `shell_id` instead of `command_id`.
    string_arg_optional(args, "command_id")
        .or_else(|| string_arg_optional(args, "shell_id"))
        .filter(|command_id| !command_id.trim().is_empty())
        .or_else(|| default_command_id.map(str::to_string))
        .unwrap_or_default()
}

fn local_tool_name(tool_call: &api::message::ToolCall) -> &'static str {
    match tool_call.tool.as_ref() {
        Some(api::message::tool_call::Tool::RunShellCommand(_)) => "run_shell_command",
        Some(api::message::tool_call::Tool::WriteToLongRunningShellCommand(_)) => {
            "write_to_long_running_shell_command"
        }
        Some(api::message::tool_call::Tool::ReadShellCommandOutput(_)) => {
            "read_shell_command_output"
        }
        Some(api::message::tool_call::Tool::TransferShellCommandControlToUser(_)) => {
            "transfer_shell_command_control_to_user"
        }
        Some(api::message::tool_call::Tool::ReadFiles(_)) => "read_files",
        Some(api::message::tool_call::Tool::Grep(_)) => "grep",
        Some(api::message::tool_call::Tool::FileGlobV2(_)) => "file_glob_v2",
        Some(api::message::tool_call::Tool::SearchCodebase(_)) => "search_codebase",
        _ => "unknown",
    }
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn int_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn array_arg<'a>(args: &'a Value, key: &str) -> Vec<&'a Value> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

fn string_array_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn truncate_tool_result(mut result: String) -> String {
    const MAX_TOOL_RESULT_CHARS: usize = 12000;
    if result.len() > MAX_TOOL_RESULT_CHARS {
        result.truncate(MAX_TOOL_RESULT_CHARS);
        result.push_str("\n...[truncated]");
    }
    result
}

fn api_keys_with_warp_credit_fallback_setting(
    api_keys: Option<api::request::settings::ApiKeys>,
    allow_use_of_warp_credits: bool,
) -> Option<api::request::settings::ApiKeys> {
    match api_keys {
        Some(mut api_keys) => {
            api_keys.allow_use_of_warp_credits = allow_use_of_warp_credits;
            Some(api_keys)
        }
        None if allow_use_of_warp_credits => Some(api::request::settings::ApiKeys {
            allow_use_of_warp_credits: true,
            ..Default::default()
        }),
        None => None,
    }
}
fn get_supported_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_tools = vec![
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
        api::ToolType::ReadMcpResource,
        api::ToolType::CallMcpTool,
        api::ToolType::InitProject,
        api::ToolType::OpenCodeReview,
        api::ToolType::RunShellCommand,
        api::ToolType::SuggestNewConversation,
        api::ToolType::Subagent,
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::ReadDocuments,
        api::ToolType::CreateDocuments,
        api::ToolType::EditDocuments,
        api::ToolType::SuggestPrompt,
    ];

    if FeatureFlag::ConversationsAsContext.is_enabled() {
        supported_tools.push(api::ToolType::FetchConversation);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_tools.extend(&[
                api::ToolType::ReadFiles,
                api::ToolType::ApplyFileDiffs,
                api::ToolType::SearchCodebase,
            ]);

            if FeatureFlag::ArtifactCommand.is_enabled() {
                supported_tools.push(api::ToolType::UploadFileArtifact);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            // Remote session with a known host — enable tools that route
            // through RemoteServerClient. The host_id is only populated
            // after a successful connection handshake, so its presence is a
            // sufficient proxy for client availability.
            supported_tools.extend(&[api::ToolType::ReadFiles, api::ToolType::ApplyFileDiffs]);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {
            // Feature flag off or not yet connected — no remote tools.
        }
    }

    if FeatureFlag::AgentModeComputerUse.is_enabled() && params.computer_use_enabled {
        supported_tools.extend(&[api::ToolType::UseComputer]);
        supported_tools.extend(&[api::ToolType::RequestComputerUse])
    }

    if FeatureFlag::PRCommentsSlashCommand.is_enabled() {
        supported_tools.push(api::ToolType::InsertReviewComments);
    }

    if FeatureFlag::ListSkills.is_enabled() {
        supported_tools.push(api::ToolType::ReadSkill);
    }

    if params.orchestration_enabled {
        // Always advertise the legacy start-agent tool so the server
        // can fall back to it when its own orchestrate flag is off.
        // When RunAgents is also enabled, advertise it alongside.
        supported_tools.push(if FeatureFlag::OrchestrationV2.is_enabled() {
            api::ToolType::StartAgentV2
        } else {
            api::ToolType::StartAgent
        });
        if FeatureFlag::RunAgentsTool.is_enabled() && FeatureFlag::OrchestrationV2.is_enabled() {
            supported_tools.push(api::ToolType::RunAgents);
        }
        supported_tools.push(api::ToolType::SendMessageToAgent);
    }

    if FeatureFlag::AskUserQuestion.is_enabled() && params.ask_user_question_enabled {
        supported_tools.push(api::ToolType::AskUserQuestion);
    }

    supported_tools
}

fn get_supported_cli_agent_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_cli_agent_tools = vec![
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
    ];

    if FeatureFlag::TransferControlTool.is_enabled() {
        supported_cli_agent_tools.push(api::ToolType::TransferShellCommandControlToUser);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_cli_agent_tools
                .extend(&[api::ToolType::ReadFiles, api::ToolType::SearchCodebase]);
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            supported_cli_agent_tools.push(api::ToolType::ReadFiles);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_cli_agent_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {}
    }

    supported_cli_agent_tools
}

#[cfg(test)]
#[path = "impl_tests.rs"]
mod tests;
