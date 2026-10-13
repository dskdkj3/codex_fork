/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

// Fix presentation input before layout in unit tests. Keep CODEX_CLI_VERSION real
// for version comparisons, update checks, and client identity; those tests pass
// explicit versions when exercising release policy.
#[cfg(not(test))]
pub(crate) const DISPLAY_VERSION: &str = CODEX_CLI_VERSION;
#[cfg(test)]
pub(crate) const DISPLAY_VERSION: &str = "0.0.0";
