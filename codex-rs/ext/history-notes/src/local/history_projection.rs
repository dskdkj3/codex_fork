use std::collections::HashMap;

use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_rollout::RolloutItem;

#[derive(Clone, Debug, Default)]
pub(crate) struct ToolCallRef {
    pub(crate) namespace: Option<String>,
    pub(crate) name: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectedItem {
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) tool_namespace: Option<String>,
    pub(crate) tool_name: Option<String>,
    pub(crate) call_id: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) agent_message_encrypted_parts: Vec<String>,
    pub(crate) content_truncated: bool,
    pub(crate) opaque: bool,
    pub(crate) unavailable_reason: Option<String>,
}

impl ProjectedItem {
    fn text(role: impl Into<String>, kind: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            kind: kind.into(),
            tool_namespace: None,
            tool_name: None,
            call_id: None,
            content: Some(content.into()),
            agent_message_encrypted_parts: Vec::new(),
            content_truncated: false,
            opaque: false,
            unavailable_reason: None,
        }
    }

    pub(super) fn opaque(role: impl Into<String>, kind: impl Into<String>, reason: &str) -> Self {
        Self {
            role: role.into(),
            kind: kind.into(),
            tool_namespace: None,
            tool_name: None,
            call_id: None,
            content: None,
            agent_message_encrypted_parts: Vec::new(),
            content_truncated: false,
            opaque: true,
            unavailable_reason: Some(reason.to_string()),
        }
    }

    fn with_tool(
        mut self,
        namespace: Option<String>,
        name: Option<String>,
        call_id: Option<String>,
    ) -> Self {
        self.tool_namespace = namespace;
        self.tool_name = name;
        self.call_id = call_id;
        self
    }

    fn with_opaque_parts(mut self, reason: &str) -> Self {
        self.opaque = true;
        self.unavailable_reason = Some(reason.to_string());
        self
    }
}

pub(crate) fn project_rollout_item(
    item: &RolloutItem,
    tool_calls: &mut HashMap<String, ToolCallRef>,
) -> Option<ProjectedItem> {
    match item {
        RolloutItem::ResponseItem(response) => {
            Some(project_response_item(&response.item, tool_calls))
        }
        RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
            let text = strip_user_message_prefix(user.message.as_str());
            if text.is_empty() {
                Some(ProjectedItem::opaque(
                    "user",
                    "user_message",
                    "empty_or_non_text",
                ))
            } else {
                Some(ProjectedItem::text("user", "user_message", text))
            }
        }
        RolloutItem::EventMsg(EventMsg::AgentMessage(agent)) => {
            if agent.message.trim().is_empty() {
                Some(ProjectedItem::opaque(
                    "assistant",
                    "agent_message",
                    "empty_or_non_text",
                ))
            } else {
                Some(ProjectedItem::text(
                    "assistant",
                    "agent_message",
                    agent.message.clone(),
                ))
            }
        }
        RolloutItem::InterAgentCommunication(message) => Some(project_inter_agent_message(message)),
        RolloutItem::SessionMeta(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::EventMsg(_) => None,
    }
}

pub(crate) fn project_response_item(
    item: &ResponseItem,
    tool_calls: &mut HashMap<String, ToolCallRef>,
) -> ProjectedItem {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let (text, has_opaque_parts) = content_text(content);
            match text {
                Some(text) if has_opaque_parts => ProjectedItem::text(role, "message", text)
                    .with_opaque_parts("mixed_text_and_non_text_content"),
                Some(text) => ProjectedItem::text(role, "message", text),
                None => ProjectedItem::opaque(role, "message", "non_text_content"),
            }
        }
        ResponseItem::AgentMessage { content, .. } => {
            let has_opaque_parts = content
                .iter()
                .any(|part| matches!(part, AgentMessageInputContent::EncryptedContent { .. }));
            let text = content
                .iter()
                .filter_map(|part| match part {
                    AgentMessageInputContent::InputText { text } => Some(text.as_str()),
                    AgentMessageInputContent::EncryptedContent { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut projection = if text.trim().is_empty() {
                ProjectedItem::opaque("assistant", "agent_message", "encrypted_or_non_text")
            } else if has_opaque_parts {
                ProjectedItem::text("assistant", "agent_message", text)
                    .with_opaque_parts("mixed_text_and_encrypted_content")
            } else {
                ProjectedItem::text("assistant", "agent_message", text)
            };
            projection.agent_message_encrypted_parts = content
                .iter()
                .filter_map(|part| match part {
                    AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                        Some(encrypted_content.clone())
                    }
                    AgentMessageInputContent::InputText { .. } => None,
                })
                .collect();
            projection
        }
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            encrypted_function_args,
            call_id,
            ..
        } => {
            remember_call(tool_calls, call_id, namespace, name);
            let projection = if encrypted_function_args.is_some() {
                ProjectedItem::opaque("tool", "function_call", "encrypted_arguments")
            } else {
                ProjectedItem::text("tool", "function_call", arguments)
            };
            projection.with_tool(namespace.clone(), Some(name.clone()), Some(call_id.clone()))
        }
        ResponseItem::FunctionCallOutput {
            call_id,
            name,
            namespace,
            output,
            ..
        } => project_tool_output(
            "function_call_output",
            call_id.as_deref(),
            name.clone(),
            namespace.clone(),
            output,
            tool_calls,
        ),
        ResponseItem::CustomToolCall {
            call_id,
            name,
            namespace,
            input,
            ..
        } => {
            remember_call(tool_calls, call_id, namespace, name);
            ProjectedItem::text("tool", "custom_tool_call", input).with_tool(
                namespace.clone(),
                Some(name.clone()),
                Some(call_id.clone()),
            )
        }
        ResponseItem::CustomToolCallOutput {
            call_id,
            name,
            output,
            ..
        } => project_tool_output(
            "custom_tool_call_output",
            Some(call_id),
            name.clone(),
            None,
            output,
            tool_calls,
        ),
        ResponseItem::LocalShellCall {
            call_id, action, ..
        } => {
            let content = match action {
                codex_protocol::models::LocalShellAction::Exec(action) => {
                    Some(action.command.join(" "))
                }
            };
            let projection = content.map_or_else(
                || ProjectedItem::opaque("tool", "local_shell_call", "non_text_tool_call"),
                |content| ProjectedItem::text("tool", "local_shell_call", content),
            );
            projection.with_tool(None, Some("local_shell".to_string()), call_id.clone())
        }
        ResponseItem::ToolSearchCall {
            call_id, arguments, ..
        } => ProjectedItem::text("tool", "tool_search_call", arguments.to_string()).with_tool(
            None,
            Some("tool_search".to_string()),
            call_id.clone(),
        ),
        ResponseItem::ToolSearchOutput { call_id, .. } => ProjectedItem::opaque(
            "tool",
            "tool_search_output",
            "non_text_tool_output",
        )
        .with_tool(None, Some("tool_search".to_string()), call_id.clone()),
        ResponseItem::AdditionalTools { .. } => {
            ProjectedItem::opaque("system", "additional_tools", "non_text_record")
        }
        ResponseItem::Reasoning { .. } => {
            ProjectedItem::opaque("assistant", "reasoning", "encrypted_or_hidden_reasoning")
        }
        ResponseItem::WebSearchCall { .. } => {
            ProjectedItem::opaque("tool", "web_search_call", "non_text_tool_call")
        }
        ResponseItem::ImageGenerationCall { .. } => {
            ProjectedItem::opaque("tool", "image_generation_call", "non_text_tool_output")
        }
        ResponseItem::Compaction { .. } => {
            ProjectedItem::opaque("system", "response_compaction", "encrypted_compaction")
        }
        ResponseItem::CompactionTrigger { .. } => {
            ProjectedItem::opaque("system", "compaction_trigger", "control_record")
        }
        ResponseItem::ContextCompaction { .. } => {
            ProjectedItem::opaque("system", "context_compaction", "encrypted_compaction")
        }
        ResponseItem::Other => ProjectedItem::opaque("system", "unknown", "unknown_record"),
    }
}

fn project_inter_agent_message(message: &InterAgentCommunication) -> ProjectedItem {
    if message.encrypted_content.is_some() {
        return ProjectedItem::opaque("assistant", "inter_agent_message", "encrypted_content");
    }
    if message.content.trim().is_empty() {
        ProjectedItem::opaque("assistant", "inter_agent_message", "empty_or_non_text")
    } else {
        ProjectedItem::text("assistant", "inter_agent_message", message.content.clone())
    }
}

fn project_tool_output(
    kind: &str,
    call_id: Option<&str>,
    explicit_name: Option<String>,
    explicit_namespace: Option<String>,
    output: &codex_protocol::models::FunctionCallOutputPayload,
    tool_calls: &mut HashMap<String, ToolCallRef>,
) -> ProjectedItem {
    let known = call_id.and_then(|id| tool_calls.get(id));
    let name = explicit_name.or_else(|| known.and_then(|call| call.name.clone()));
    let namespace = explicit_namespace.or_else(|| known.and_then(|call| call.namespace.clone()));
    let projection = match &output.body {
        FunctionCallOutputBody::Text(text) => ProjectedItem::text("tool", kind, text),
        FunctionCallOutputBody::ContentItems(items) => {
            let text = items
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                    FunctionCallOutputContentItem::InputImage { .. }
                    | FunctionCallOutputContentItem::InputAudio { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let has_opaque_parts = items
                .iter()
                .any(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }));
            if text.trim().is_empty() {
                ProjectedItem::opaque("tool", kind, "encrypted_or_non_text_output")
            } else if has_opaque_parts {
                ProjectedItem::text("tool", kind, text)
                    .with_opaque_parts("mixed_text_and_non_text_output")
            } else {
                ProjectedItem::text("tool", kind, text)
            }
        }
    };
    projection.with_tool(namespace, name, call_id.map(str::to_string))
}

fn remember_call(
    tool_calls: &mut HashMap<String, ToolCallRef>,
    call_id: &str,
    namespace: &Option<String>,
    name: &str,
) {
    if tool_calls.len() < super::limits::MAX_HISTORY_TOOL_CALLS {
        tool_calls.insert(
            call_id.to_string(),
            ToolCallRef {
                namespace: namespace.clone(),
                name: Some(name.to_string()),
            },
        );
    }
}

fn content_text(content: &[ContentItem]) -> (Option<String>, bool) {
    let text = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let has_opaque_parts = content.iter().any(|item| {
        matches!(
            item,
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. }
        )
    });
    if text.trim().is_empty() {
        (None, has_opaque_parts)
    } else {
        (Some(text), has_opaque_parts)
    }
}
