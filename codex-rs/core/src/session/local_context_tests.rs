use std::fs;
use std::sync::Arc;

use codex_features::ContextManagementBackend;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::activate;
use super::enabled;
use super::prepare;
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

#[tokio::test]
async fn local_context_activates_without_official_auth_and_excludes_children() {
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

    activate(
        &mut config,
        &SessionSource::SubAgent(SubAgentSource::Compact),
    )
    .unwrap();
    assert!(!enabled(&config));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(config.token_budget, None);
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
    let resumed = InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(Vec::new()),
        rollout_path: None,
    });
    assert!(prepare(&config, &resumed, &SessionSource::Cli, thread_id).is_err());
    prepare(
        &config,
        &InitialHistory::New,
        &SessionSource::Cli,
        thread_id,
    )
    .unwrap();
    prepare(&config, &resumed, &SessionSource::Cli, thread_id).unwrap();
    assert!(prepare(&config, &resumed, &SessionSource::Cli, ThreadId::new()).is_err());
    config.context_management_backend = ContextManagementBackend::Codex;
    assert!(prepare(&config, &resumed, &SessionSource::Cli, thread_id).is_err());
}

#[tokio::test]
async fn local_context_rejects_invalid_manifest_and_ephemeral_threads() {
    let home = TempDir::new().unwrap();
    let mut config = local_config(&home, "").await;
    config.ephemeral = true;
    assert!(activate(&mut config, &SessionSource::Cli).is_err());
    config.ephemeral = false;
    let thread_id = ThreadId::new();
    prepare(
        &config,
        &InitialHistory::New,
        &SessionSource::Cli,
        thread_id,
    )
    .unwrap();
    fs::write(
        home.path()
            .join("context-management-local")
            .join(thread_id.to_string())
            .join("backend.json"),
        "{\"version\":999}",
    )
    .unwrap();
    let resumed = InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(Vec::new()),
        rollout_path: None,
    });
    assert!(prepare(&config, &resumed, &SessionSource::Cli, thread_id).is_err());
    assert!(
        prepare(
            &config,
            &InitialHistory::Forked(Vec::new()),
            &SessionSource::Cli,
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
    prepare(&first, &InitialHistory::New, &SessionSource::Cli, thread_id).unwrap();
    let resumed = InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(Vec::new()),
        rollout_path: None,
    });
    prepare(&second, &resumed, &SessionSource::Cli, thread_id).unwrap();
    assert!(
        durable
            .join(thread_id.to_string())
            .join("backend.json")
            .is_file()
    );
    assert!(!second_home.path().join("context-management-local").exists());

    let wrong_store = local_config(&second_home, "").await;
    assert!(prepare(&wrong_store, &resumed, &SessionSource::Cli, thread_id).is_err());
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
    assert!(
        prepare(
            &config,
            &InitialHistory::New,
            &SessionSource::Cli,
            ThreadId::new()
        )
        .is_err()
    );
    assert!(!target.path().join("state").exists());
}
