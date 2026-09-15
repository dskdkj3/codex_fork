//! Local context activation and thread recovery compatibility.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use codex_features::ContextManagementBackend;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde_json::json;

use crate::config::Config;
use crate::config::resolve_token_budget_config;

const MANIFEST_LIMIT: u64 = 1024;

pub(super) fn supports_source(source: &SessionSource) -> bool {
    !source.is_non_root_agent()
        || matches!(
            source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
}

pub(super) fn enabled(config: &Config) -> bool {
    config.context_management_backend == ContextManagementBackend::Local
        && config.features.enabled(Feature::ContextManagement)
}

pub(super) fn resolve_resumed_backend(
    config: &mut Config,
    history: &InitialHistory,
    source: &SessionSource,
) -> io::Result<Option<ContextManagementBackend>> {
    if !matches!(history, InitialHistory::Resumed(_)) {
        return Ok(None);
    }

    let inherited_local_backend =
        config.context_management_backend == ContextManagementBackend::Local;
    let recorded_backend = history.get_resumed_context_management_backend();
    let recorded_source_is_supported = history
        .get_rollout_items()
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(supports_source(&meta.meta.source)),
            _ => None,
        })
        .unwrap_or(true);
    if recorded_backend == Some(ContextManagementBackend::Local)
        && (!supports_source(source) || !recorded_source_is_supported)
    {
        return Err(io::Error::other(
            "local context does not support this internal thread source",
        ));
    }
    if let Some(backend) = recorded_backend {
        config.context_management_backend = backend;
    } else if source.is_non_root_agent() {
        config.context_management_backend = ContextManagementBackend::Codex;
    }

    // Codex also represents the official experimental backend. Only override its
    // activation state when removing inherited local state; otherwise keep the
    // existing official eligibility and explicit opt-in path.
    let resumed_child_backend = (source.is_non_root_agent()
        && (inherited_local_backend || recorded_backend == Some(ContextManagementBackend::Local)))
    .then_some(recorded_backend.unwrap_or(ContextManagementBackend::Codex));
    if let Some(backend) = resumed_child_backend {
        match backend {
            ContextManagementBackend::Local => {
                config
                    .features
                    .enable(Feature::ContextManagement)
                    .map_err(|err| io::Error::other(err.to_string()))?;
            }
            ContextManagementBackend::Codex => {
                config
                    .features
                    .disable(Feature::ContextManagement)
                    .map_err(|err| io::Error::other(err.to_string()))?;
                config
                    .features
                    .disable(Feature::TokenBudget)
                    .map_err(|err| io::Error::other(err.to_string()))?;
                config.token_budget = None;
            }
        }
    }
    Ok(resumed_child_backend)
}

pub(super) fn resolved_backend(config: &Config) -> ContextManagementBackend {
    if enabled(config) {
        ContextManagementBackend::Local
    } else {
        ContextManagementBackend::Codex
    }
}

pub(super) fn activate(config: &mut Config, source: &SessionSource) -> io::Result<()> {
    if !enabled(config) {
        return Ok(());
    }
    if !supports_source(source) {
        config.context_management_backend = ContextManagementBackend::Codex;
        config
            .features
            .disable(Feature::ContextManagement)
            .map_err(|err| io::Error::other(err.to_string()))?;
        config
            .features
            .disable(Feature::TokenBudget)
            .map_err(|err| io::Error::other(err.to_string()))?;
        config.token_budget = None;
        return Ok(());
    }
    if config.ephemeral {
        return Err(io::Error::other(
            "local context requires a persistent thread",
        ));
    }
    let effective = config.config_layer_stack.effective_config();
    if let Some(settings) = effective
        .get("features")
        .and_then(|f| f.get("token_budget"))
        && (settings.as_bool() == Some(false)
            || settings.get("enabled").and_then(toml::Value::as_bool) == Some(false)
            || settings
                .get("use_history_notes_extension")
                .and_then(toml::Value::as_bool)
                == Some(false))
    {
        return Err(io::Error::other(
            "local context conflicts with explicitly disabled token-budget or history-notes settings",
        ));
    }
    config
        .features
        .enable(Feature::TokenBudget)
        .map_err(|err| io::Error::other(err.to_string()))?;
    if !config.features.enabled(Feature::TokenBudget) {
        return Err(io::Error::other(
            "local context requires token-budget permission",
        ));
    }
    if config.token_budget.is_none() {
        let config_toml = effective
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        config.token_budget = resolve_token_budget_config(&config_toml, &config.features)?;
    }
    config
        .token_budget
        .get_or_insert_default()
        .use_history_notes_extension = true;
    Ok(())
}

/// Bind local recovery to a new thread, or verify an existing thread's mode.
/// Only the host's resolved identity is accepted, never a tool argument.
pub(super) fn prepare(
    config: &Config,
    history: &InitialHistory,
    thread_id: ThreadId,
) -> io::Result<()> {
    let local = enabled(config);
    let resumed = matches!(history, InitialHistory::Resumed(_));
    if !local && !resumed {
        return Ok(());
    }
    if local
        && (matches!(history, InitialHistory::Forked(_))
            || history.get_rollout_items().iter().any(|item| {
                matches!(item, RolloutItem::SessionMeta(meta)
                    if meta.meta.history_base.is_some() || meta.meta.forked_from_id.is_some())
            }))
    {
        return Err(io::Error::other(
            "local context does not support inherited fork history",
        ));
    }
    let root = config.context_management_local_store_dir.as_path();
    let thread_root = root.join(thread_id.to_string());
    // Validate every ancestor before creating an explicitly selected local directory. Ordinary
    // official resumes retain the original root/marker-only compatibility check.
    let directories = if local {
        root.ancestors()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        vec![root]
    };
    for directory in directories
        .into_iter()
        .chain(std::iter::once(thread_root.as_path()))
    {
        match fs::symlink_metadata(directory) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(io::Error::other("invalid local context directory"));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if !local || resumed {
                    return if local {
                        Err(io::Error::other(
                            "thread has no compatible local context record",
                        ))
                    } else {
                        Ok(())
                    };
                }
                create_private_directory(directory)?;
            }
            Err(err) => return Err(err),
        }
    }
    let manifest_path = thread_root.join("backend.json");
    let expected = json!({"version": 1, "backend": "local", "thread_id": thread_id.to_string()});
    if resumed {
        let meta = match fs::symlink_metadata(&manifest_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound && !local => return Ok(()),
            Err(err) => return Err(err),
        };
        if !local {
            return Err(io::Error::other(
                "resume requires the recorded local context backend",
            ));
        }
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MANIFEST_LIMIT {
            return Err(io::Error::other("invalid local context record"));
        }
        let mut contents = Vec::new();
        fs::File::open(&manifest_path)?
            .take(MANIFEST_LIMIT + 1)
            .read_to_end(&mut contents)?;
        if contents.len() > MANIFEST_LIMIT as usize
            || serde_json::from_slice::<serde_json::Value>(&contents)
                .ok()
                .as_ref()
                != Some(&expected)
        {
            return Err(io::Error::other("incompatible local context record"));
        }
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&manifest_path)?;
    file.write_all(expected.to_string().as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
#[path = "local_context_tests.rs"]
mod tests;
