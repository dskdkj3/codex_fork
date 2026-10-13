use codex_protocol::ModelContextOverride;
use codex_protocol::ModelContextPreset;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ModelsResponse;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct ModelsManagerConfig {
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub model_context_overrides: BTreeMap<String, ModelContextOverride>,
    pub model_context_presets: BTreeMap<String, ModelContextPreset>,
    pub tool_output_token_limit: Option<usize>,
    pub base_instructions: Option<String>,
    pub personality: Option<Personality>,
    pub model_catalog: Option<ModelsResponse>,
}
