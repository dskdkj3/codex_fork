use crate::ModelsManagerConfig;
use crate::manager::ModelsManager;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::TestModelsEndpoint;
use super::openai_manager_for_tests;

#[tokio::test]
async fn context_preset_tracks_base_metadata_and_keeps_distinct_limits() {
    let codex_home = TempDir::new().expect("create temp dir");
    let mut base = super::remote_model("test-astra", "Astra", /*priority*/ 0);
    base.max_context_window = Some(900_000);
    base.visibility = codex_protocol::openai_models::ModelVisibility::Hide;
    let mut refreshed = base.clone();
    refreshed.service_tiers = vec![codex_protocol::openai_models::ModelServiceTier {
        id: "priority".to_string(),
        name: "Fast".to_string(),
        description: "Updated remote tier".to_string(),
    }];
    let endpoint = TestModelsEndpoint::new(vec![vec![base], vec![refreshed.clone()], vec![]]);
    let config = ModelsManagerConfig {
        model_context_overrides: serde_json::from_value(serde_json::json!({
            "test-astra": {"context_window": 800_000, "auto_compact_token_limit": 250_000},
            "long/test-astra": {"auto_compact_token_limit": 500_000}
        }))
        .unwrap(),
        model_context_presets: serde_json::from_value(serde_json::json!({
            "long/test-astra": {"display_name": "Astra (Long context)"}
        }))
        .unwrap(),
        ..Default::default()
    };
    let manager = crate::with_context_presets(
        std::sync::Arc::new(openai_manager_for_tests(
            codex_home.path().to_path_buf(),
            endpoint,
        )),
        config.model_context_presets.clone(),
    );
    for _ in 0..2 {
        manager
            .list_models(
                super::RefreshStrategy::Online,
                super::DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await;
    }
    let preset = manager
        .try_list_models()
        .unwrap()
        .into_iter()
        .find(|model| model.model == "long/test-astra")
        .unwrap();
    assert_eq!(preset.display_name, "Astra (Long context)");
    assert!(preset.show_in_picker);
    assert_eq!(preset.service_tiers, refreshed.service_tiers);

    for (selected, limit, upstream) in [
        ("test-astra", 250_000, None),
        ("long/test-astra", 500_000, Some("test-astra")),
        ("test-astra", 250_000, None),
    ] {
        let actual = manager.get_model_info(selected, &config).await;
        let mut expected = refreshed.clone();
        expected.slug = selected.to_string();
        expected.request_model = upstream.map(str::to_string);
        expected.context_window = Some(800_000);
        expected.auto_compact_token_limit = Some(limit);
        assert_eq!(actual, expected);
    }
    // Removing a provider model also removes the local picker entry.
    let models = manager
        .list_models(
            super::RefreshStrategy::Online,
            super::DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await;
    assert!(!models.iter().any(|model| model.model == "long/test-astra"));
    // Explicit selection still never sends the local preset ID upstream.
    let fallback = manager.get_model_info("long/test-astra", &config).await;
    assert_eq!(fallback.request_model(), "test-astra");
    assert!(fallback.used_fallback_model_metadata);
}

#[tokio::test]
async fn model_context_policy_keeps_remote_discovery_and_fast_tiers() {
    let codex_home = TempDir::new().expect("create temp dir");
    let original = super::remote_model("test-sol", "Sol", /*priority*/ 0);
    let mut refreshed = original.clone();
    refreshed.context_window = Some(500_000);
    refreshed.max_context_window = Some(800_000);
    let mut future = super::remote_model("test-sol-next", "Next Sol", /*priority*/ 1);
    future.service_tiers = vec![codex_protocol::openai_models::ModelServiceTier {
        id: "priority".to_string(),
        name: "Fast".to_string(),
        description: "New remote capability".to_string(),
    }];
    let endpoint = TestModelsEndpoint::new(vec![
        vec![original],
        vec![refreshed.clone(), future.clone()],
    ]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());
    let config = ModelsManagerConfig {
        model_context_overrides: serde_json::from_value(serde_json::json!({
            "test-sol": {"auto_compact_token_limit": 190_000}
        }))
        .unwrap(),
        ..Default::default()
    };
    for _ in 0..2 {
        manager
            .list_models(
                super::RefreshStrategy::Online,
                super::DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await;
    }

    refreshed.auto_compact_token_limit = Some(190_000);
    assert_eq!(manager.get_model_info("test-sol", &config).await, refreshed);
    assert_eq!(
        manager.get_model_info("test-sol-next", &config).await,
        future
    );
    assert_eq!(endpoint.fetch_count(), 2);
    let preset = manager
        .try_list_models()
        .unwrap()
        .into_iter()
        .find(|model| model.model == "test-sol-next")
        .unwrap();
    assert_eq!(preset.service_tiers, future.service_tiers);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_model_info_without_tool_output_override() {
    let codex_home = TempDir::new().expect("create temp dir");
    let config = ModelsManagerConfig::default();
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );

    let model_info = manager.get_model_info("unknown-model", &config).await;

    assert_eq!(
        model_info.truncation_policy,
        TruncationPolicyConfig::bytes(/*limit*/ 10_000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_model_info_with_tool_output_override() {
    let codex_home = TempDir::new().expect("create temp dir");
    let config = ModelsManagerConfig {
        tool_output_token_limit: Some(123),
        ..Default::default()
    };
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );

    let model_info = manager.get_model_info("gpt-5.5", &config).await;

    assert_eq!(
        model_info.truncation_policy,
        TruncationPolicyConfig::tokens(/*limit*/ 123)
    );
}
