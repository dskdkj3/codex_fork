use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::num::NonZeroU32;

/// Context policy for one exact model slug, independent of model discovery.
/// Unspecified fields retain provider metadata; global context overrides take precedence.
#[derive(Debug, Serialize, Deserialize, Default, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelContextOverride {
    /// Requested context window, still bounded by the provider's maximum window.
    pub context_window: Option<NonZeroU32>,
    /// Auto-compaction threshold, still bounded by the model's effective context cap.
    pub auto_compact_token_limit: Option<NonZeroU32>,
}

/// A local picker entry named `preset/model-slug`, using the suffix for requests.
/// Capabilities are always inherited from the provider's exact model entry.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelContextPreset {
    pub display_name: String,
}
