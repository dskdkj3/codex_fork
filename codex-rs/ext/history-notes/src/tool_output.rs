use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonToolOutput;
use serde_json::Value;
use serde_json::json;

pub(crate) struct HistoryNotesToolOutput {
    pub(crate) redact_observers: bool,
    result: Value,
    output: FunctionCallOutputPayload,
}

impl HistoryNotesToolOutput {
    pub(crate) fn new(mut result: Value) -> Result<Self, FunctionCallError> {
        // Separate attachments before serializing any text or retaining log output.
        let images = result.as_object_mut().and_then(|map| map.remove("images"));
        // The server applies the requested output budget before encryption.
        let mut output = match result.get("encrypted_output").and_then(Value::as_str) {
            Some(encrypted_content) => FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: encrypted_content.to_string(),
                },
            ]),
            None => FunctionCallOutputPayload::from_text(result.to_string()),
        };
        if let Some(images) = images {
            let invalid_image = || {
                FunctionCallError::RespondToModel(
                    "History backend returned invalid image content.".to_string(),
                )
            };
            let images = images.as_array().ok_or_else(invalid_image)?;
            let mut content = match output.body {
                FunctionCallOutputBody::Text(text) => {
                    vec![FunctionCallOutputContentItem::InputText { text }]
                }
                FunctionCallOutputBody::ContentItems(content) => content,
            };
            for image in images {
                let data = image
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_image)?;
                let mime_type = image
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_image)?;
                let detail =
                    serde_json::from_value(image.get("detail").cloned().unwrap_or(Value::Null))
                        .map_err(|_| invalid_image())?;
                content.push(FunctionCallOutputContentItem::InputImage {
                    image_url: format!("data:{mime_type};base64,{data}"),
                    detail,
                });
            }
            output = FunctionCallOutputPayload::from_content_items(content);
        }
        Ok(Self {
            result,
            output,
            redact_observers: false,
        })
    }
}

impl HistoryNotesToolOutput {
    fn observer_result(&self) -> Value {
        if self.redact_observers {
            json!({"local_context_private": true})
        } else {
            self.result.clone()
        }
    }
}

impl ToolOutput for HistoryNotesToolOutput {
    fn log_output(&self) -> String {
        JsonToolOutput::new(self.observer_result()).log_output()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<Value> {
        self.redact_observers
            .then(|| json!({"local_context_private": true}))
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<Value> {
        // Hooks must not receive model-only image attachments.
        Some(self.observer_result())
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.output.clone(),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        Value::String("History tools are unavailable in code mode.".to_string())
    }
}
