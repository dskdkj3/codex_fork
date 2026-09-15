use std::fs;
use std::sync::Arc;

use codex_features::ContextManagementBackend;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::activate;
use super::enabled;
use super::prepare;
use super::resolve_resumed_backend;
use crate::config::Config;
use crate::config::ConfigBuilder;

async fn local_config(home: &TempDir, additional: &str) -> Config {
    fs::write(
        home.path().join("config.toml"),
        format!(
            "[features.context_management]\nexperimental_mode = true\nbackend = 'local'\n{additional}"
        ),
    )
    .expect("write synthetic config");
    ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .expect("load synthetic config")
}

fn spawned_child_source() -> SessionSource {
    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    })
}

fn resumed_history(
    thread_id: ThreadId,
    backend: Option<ContextManagementBackend>,
) -> InitialHistory {
    InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(vec![codex_history::RolloutItem::SessionMeta(
            SessionMetaLine {
                meta: SessionMeta {
                    id: thread_id,
                    session_id: thread_id.into(),
                    context_management_backend: backend,
                    ..SessionMeta::default()
                },
                git: None,
            },
        )]),
        rollout_path: None,
    })
}

#[tokio::test]
async fn local_context_activates_without_official_auth_for_fresh_children() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    config.model_provider.requires_openai_auth = false;
    activate(&mut config, &SessionSource::Cli).unwrap();
    assert!(enabled(&config));
    assert!(config.features.enabled(Feature::TokenBudget));
    assert!(
        config
            .token_budget
            .as_ref()
            .unwrap()
            .use_history_notes_extension
    );

    activate(&mut config, &spawned_child_source()).unwrap();
    assert!(enabled(&config));
    assert!(config.features.enabled(Feature::TokenBudget));
    assert!(config.token_budget.is_some());
}

#[tokio::test]
async fn resumed_children_use_their_own_recorded_backend() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    activate(&mut config, &SessionSource::Cli).unwrap();
    assert!(enabled(&config));
    assert!(config.features.enabled(Feature::TokenBudget));
    assert!(config.token_budget.is_some());
    let thread_id = ThreadId::new();
    let child = spawned_child_source();

    let historical = resumed_history(thread_id, None);
    let _ = resolve_resumed_backend(&mut config, &historical, &child).unwrap();
    assert_eq!(
        config.context_management_backend,
        ContextManagementBackend::Codex
    );
    assert!(!config.features.enabled(Feature::ContextManagement));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(config.token_budget, None);

    let local = resumed_history(thread_id, Some(ContextManagementBackend::Local));
    let _ = resolve_resumed_backend(&mut config, &local, &child).unwrap();
    assert_eq!(
        config.context_management_backend,
        ContextManagementBackend::Local
    );
    assert!(config.features.enabled(Feature::ContextManagement));
    activate(&mut config, &child).unwrap();
    assert!(config.features.enabled(Feature::TokenBudget));
    assert!(config.token_budget.is_some());
    assert!(prepare(&config, &local, thread_id).is_err());
    prepare(&config, &InitialHistory::New, thread_id).unwrap();
    prepare(&config, &local, thread_id).unwrap();

    let codex = resumed_history(thread_id, Some(ContextManagementBackend::Codex));
    let _ = resolve_resumed_backend(&mut config, &codex, &child).unwrap();
    assert_eq!(
        config.context_management_backend,
        ContextManagementBackend::Codex
    );
    assert!(!config.features.enabled(Feature::ContextManagement));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(config.token_budget, None);
    assert!(prepare(&config, &codex, thread_id).is_err());
}

#[tokio::test]
async fn local_context_excludes_internal_sources_and_rejects_recorded_local_recovery() {
    for source in [
        SessionSource::SubAgent(SubAgentSource::Review),
        SessionSource::SubAgent(SubAgentSource::Compact),
        SessionSource::SubAgent(SubAgentSource::Other("fixture".to_string())),
        SessionSource::SubAgent(SubAgentSource::MemoryConsolidation),
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
    ] {
        let home = TempDir::new().unwrap();
        let mut config = local_config(&home, "").await;
        activate(&mut config, &SessionSource::Cli).unwrap();
        activate(&mut config, &source).unwrap();
        assert_eq!(
            config.context_management_backend,
            ContextManagementBackend::Codex
        );
        assert!(!config.features.enabled(Feature::ContextManagement));
        assert!(!config.features.enabled(Feature::TokenBudget));
        assert!(config.token_budget.is_none());
        let thread_id = ThreadId::new();
        prepare(&config, &InitialHistory::New, thread_id).unwrap();
        assert!(
            !config
                .context_management_local_store_dir
                .join(thread_id.to_string())
                .exists()
        );
        let local = resumed_history(thread_id, Some(ContextManagementBackend::Local));
        assert!(resolve_resumed_backend(&mut config, &local, &source).is_err());
        let mut recorded_internal = local;
        let InitialHistory::Resumed(ref mut resumed) = recorded_internal else {
            panic!("expected resumed fixture");
        };
        let codex_history::RolloutItem::SessionMeta(meta) =
            &mut Arc::make_mut(&mut resumed.history)[0]
        else {
            panic!("expected canonical metadata");
        };
        meta.meta.source = source;
        assert!(
            resolve_resumed_backend(&mut config, &recorded_internal, &SessionSource::Cli).is_err()
        );
    }
}

#[tokio::test]
async fn resumed_official_children_preserve_existing_experimental_activation_state() {
    for backend in [None, Some(ContextManagementBackend::Codex)] {
        for source in [
            spawned_child_source(),
            SessionSource::SubAgent(SubAgentSource::Review),
        ] {
            let home = TempDir::new().unwrap();
            let mut config = local_config(&home, "").await;
            // Construct the already-activated flag state produced by official
            // eligibility. This test does not claim a live OAuth/authentication probe.
            activate(&mut config, &SessionSource::Cli).unwrap();
            config.context_management_backend = ContextManagementBackend::Codex;
            let original_budget = config.token_budget.clone();
            let history = resumed_history(ThreadId::new(), backend);
            assert_eq!(
                resolve_resumed_backend(&mut config, &history, &source).unwrap(),
                None
            );
            assert_eq!(
                config.context_management_backend,
                ContextManagementBackend::Codex
            );
            assert!(config.features.enabled(Feature::ContextManagement));
            assert!(config.features.enabled(Feature::TokenBudget));
            assert_eq!(config.token_budget, original_budget);
        }
    }
}

#[tokio::test]
async fn historical_root_keeps_m6_config_and_marker_compatibility() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    let thread_id = ThreadId::new();
    prepare(&config, &InitialHistory::New, thread_id).unwrap();
    let historical = resumed_history(thread_id, None);
    let _ = resolve_resumed_backend(&mut config, &historical, &SessionSource::Cli).unwrap();
    assert_eq!(
        config.context_management_backend,
        ContextManagementBackend::Local
    );
    prepare(&config, &historical, thread_id).unwrap();
}

#[tokio::test]
async fn local_context_rejects_explicit_low_level_conflicts() {
    for setting in ["enabled = false", "use_history_notes_extension = false"] {
        let home = TempDir::new().unwrap();
        let mut config =
            local_config(&home, &format!("\n[features.token_budget]\n{setting}\n")).await;
        let error = activate(&mut config, &SessionSource::Cli).unwrap_err();
        assert!(error.to_string().contains("conflicts"));
    }
}

#[tokio::test]
async fn local_context_resume_requires_same_persisted_backend_and_thread() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    let thread_id = ThreadId::new();
    let resumed = resumed_history(thread_id, Some(ContextManagementBackend::Local));
    assert!(prepare(&config, &resumed, thread_id).is_err());
    prepare(&config, &InitialHistory::New, thread_id).unwrap();
    prepare(&config, &resumed, thread_id).unwrap();
    assert!(prepare(&config, &resumed, ThreadId::new()).is_err());
    config.context_management_backend = ContextManagementBackend::Codex;
    assert!(prepare(&config, &resumed, thread_id).is_err());
}

#[tokio::test]
async fn local_context_rejects_invalid_manifest_and_ephemeral_threads() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    config.ephemeral = true;
    assert!(activate(&mut config, &SessionSource::Cli).is_err());
    config.ephemeral = false;
    let thread_id = ThreadId::new();
    prepare(&config, &InitialHistory::New, thread_id).unwrap();
    fs::write(
        home.path()
            .join("context-management-local")
            .join(thread_id.to_string())
            .join("backend.json"),
        "{\"version\":999}",
    )
    .unwrap();
    let resumed = resumed_history(thread_id, Some(ContextManagementBackend::Local));
    assert!(prepare(&config, &resumed, thread_id).is_err());
    assert!(
        prepare(
            &config,
            &InitialHistory::Forked(Vec::new()),
            ThreadId::new()
        )
        .is_err()
    );
}

#[tokio::test]
async fn local_context_explicit_store_survives_different_homes() {
    let first_home = TempDir::new().unwrap();
    let second_home = TempDir::new().unwrap();
    let durable_parent = TempDir::new().unwrap();
    let durable = durable_parent.path().join("state").join("local");
    let setting = format!(
        "local_store_dir = {}\n",
        toml::Value::String(durable.to_str().unwrap().to_string())
    );
    let first = local_config(&first_home, &setting).await;
    let second = local_config(&second_home, &setting).await;
    assert_eq!(first.context_management_local_store_dir.as_path(), durable);
    assert_eq!(
        first.context_management_local_store_dir,
        second.context_management_local_store_dir
    );
    let thread_id = ThreadId::new();
    prepare(&first, &InitialHistory::New, thread_id).unwrap();
    let resumed = resumed_history(thread_id, Some(ContextManagementBackend::Local));
    prepare(&second, &resumed, thread_id).unwrap();
    assert!(
        durable
            .join(thread_id.to_string())
            .join("backend.json")
            .is_file()
    );
    assert!(!second_home.path().join("context-management-local").exists());

    let wrong_store = local_config(&second_home, "").await;
    assert!(prepare(&wrong_store, &resumed, thread_id).is_err());
    assert!(!second_home.path().join("context-management-local").exists());
}

#[tokio::test]
async fn local_context_rejects_nonabsolute_or_unnormalized_store_config() {
    let home = TempDir::new().unwrap();
    for path in [
        "relative/state".to_string(),
        format!("{}/../state", home.path().display()),
        format!("{}/./state", home.path().display()),
    ] {
        fs::write(
            home.path().join("config.toml"),
            format!(
                "[features.context_management]\nbackend = 'local'\nlocal_store_dir = {}\n",
                toml::Value::String(path),
            ),
        )
        .unwrap();
        let error = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("local_store_dir"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn local_context_rejects_symlinked_explicit_store_ancestor() {
    let home = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let alias = home.path().join("alias");
    std::os::unix::fs::symlink(target.path(), &alias).unwrap();
    let store = alias.join("state");
    let config = local_config(
        &home,
        &format!(
            "local_store_dir = {}\n",
            toml::Value::String(store.to_str().unwrap().to_string()),
        ),
    )
    .await;
    assert!(prepare(&config, &InitialHistory::New, ThreadId::new()).is_err());
    assert!(!target.path().join("state").exists());
}
