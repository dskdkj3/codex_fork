//! Metadata-only observer views for opt-in local context tools.
//! Hook execution and blocking remain enabled; native thread history stays intact.

use std::borrow::Cow;

use codex_features::ContextManagementBackend;
use codex_features::Feature;
use serde_json::Value;
use serde_json::json;

use super::context::ToolInvocation;
use super::router::tool_log_payload;

pub(super) fn hook_input(invocation: &ToolInvocation) -> Option<Value> {
    let config = &invocation.turn.config;
    (config.context_management_backend == ContextManagementBackend::Local
        && config.features.enabled(Feature::ContextManagement)
        && matches!(
            invocation.tool_name.namespace.as_deref(),
            Some("history" | "notes")
        ))
    .then(|| json!({"local_context_private": true}))
}

pub(super) fn log_payload(invocation: &ToolInvocation) -> Cow<'_, str> {
    hook_input(invocation).map_or_else(
        || tool_log_payload(&invocation.payload, &invocation.source),
        |metadata| Cow::Owned(metadata.to_string()),
    )
}
