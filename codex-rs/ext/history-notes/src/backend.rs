use std::sync::Arc;
use std::time::Duration;

use crate::local::LocalHistoryNotesStore;
use codex_api::ReqwestTransport;
use codex_client::HttpTransport;
use codex_client::RequestBody;
use codex_login::default_client::create_client;
use codex_model_provider::SharedModelProvider;
use codex_utils_output_truncation::TruncationPolicy;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use serde_json::json;

const HISTORY_NOTES_BACKEND_TIMEOUT: Duration = Duration::from_secs(35);
const ENCRYPTED_TOOL_ARGUMENTS_HEADER: &str = "x-openai-encrypted-tool-arguments";
const TOOL_OUTPUT_TRUNCATION_POLICY_HEADER: &str = "x-openai-tool-output-truncation-policy";
const OPERATION_ERROR_PREFIX: &str = "Unable to perform operation:";

#[derive(Clone)]
pub(crate) struct HistoryNotesBackend {
    kind: BackendKind,
}

#[derive(Clone)]
enum BackendKind {
    Codex(SharedModelProvider),
    Local(Arc<LocalHistoryNotesStore>),
}

impl HistoryNotesBackend {
    pub(crate) fn new(provider: SharedModelProvider) -> Self {
        Self {
            kind: BackendKind::Codex(provider),
        }
    }

    pub(crate) fn local(store: LocalHistoryNotesStore) -> Self {
        Self {
            kind: BackendKind::Local(Arc::new(store)),
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        matches!(self.kind, BackendKind::Local(_))
    }

    pub(crate) async fn call(
        &self,
        path: &str,
        session_id: &str,
        current_agent_name: &str,
        mut arguments: Value,
        truncation_policy: TruncationPolicy,
    ) -> Result<Value, String> {
        if !arguments.is_object() {
            return Err("History tool arguments must be a JSON object".to_string());
        }
        let provider = match &self.kind {
            BackendKind::Local(store) => {
                let result = if path == "alpha/notes/v2/thread_hint" {
                    store.thread_hint()
                } else {
                    store.call(path, arguments).await?
                };
                if result.to_string().len() > 8_000 {
                    return Err(
                        "Local context result exceeds the output limit; request a smaller range."
                            .to_string(),
                    );
                }
                return Ok(result);
            }
            BackendKind::Codex(provider) => provider,
        };
        let Some(arguments_object) = arguments.as_object_mut() else {
            return Err("History tool arguments must be a JSON object".to_string());
        };
        arguments_object.insert(
            "context".to_string(),
            json!({
                "session_id": session_id,
                "current_agent_name": current_agent_name,
            }),
        );

        let api_provider = provider.api_provider().await.map_err(|_| {
            format!("{OPERATION_ERROR_PREFIX} Could not resolve the backend provider.")
        })?;
        let auth = provider.api_auth().await.map_err(|_| {
            format!("{OPERATION_ERROR_PREFIX} Could not resolve backend authentication.")
        })?;

        let mut request = api_provider.build_request(Method::POST, path);
        let encoded_truncation_policy =
            serde_json::to_string(&truncation_policy).map_err(|_| {
                format!("{OPERATION_ERROR_PREFIX} Could not encode the output truncation policy.")
            })?;
        request.headers.insert(
            TOOL_OUTPUT_TRUNCATION_POLICY_HEADER,
            HeaderValue::from_str(&encoded_truncation_policy).map_err(|_| {
                format!(
                    "{OPERATION_ERROR_PREFIX} Could not construct the output truncation policy header."
                )
            })?,
        );
        if matches!(
            path,
            "alpha/history/v2/search_contents"
                | "alpha/notes/v2/search_contents"
                | "alpha/notes/v2/append_to_file"
                | "alpha/notes/v2/write_file"
        ) {
            request.headers.insert(
                ENCRYPTED_TOOL_ARGUMENTS_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        request.body = Some(RequestBody::Json(arguments));
        request.timeout = Some(HISTORY_NOTES_BACKEND_TIMEOUT);
        let request = auth.apply_auth(request).await.map_err(|_| {
            format!("{OPERATION_ERROR_PREFIX} Could not apply backend authentication.")
        })?;
        let response = ReqwestTransport::from_http_client(create_client())
            .execute(request)
            .await
            .map_err(|_| format!("{OPERATION_ERROR_PREFIX} The backend request failed."))?;

        serde_json::from_slice(&response.body)
            .map_err(|_| format!("{OPERATION_ERROR_PREFIX} The backend returned invalid JSON."))
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
