use std::sync::Arc;

use codex_core::config::Config;
use codex_core::config::TokenBudgetConfig;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::ContextManagementBackend;
use codex_features::Feature;
use codex_history_notes_extension::install;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const USER_CONSTRAINT: &str = "retain_original_constraint_71c9";
const TOOL_ERROR: &str = "original_tool_error_29af";

fn response(id: &str, event: Value) -> String {
    sse(vec![ev_response_created(id), event, ev_completed(id)])
}

fn output(
    requests: &[ResponsesRequest],
    call_id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let text = requests
        .iter()
        .find_map(|request| request.function_call_output_text(call_id))
        .ok_or("native tool output must reach the next model request")?;
    Ok(serde_json::from_str(&text)?)
}

fn configure_local(config: &mut Config) {
    config.context_management_backend = ContextManagementBackend::Local;
    assert!(config.features.enable(Feature::ContextManagement).is_ok());
    config.model_provider.requires_openai_auth = false;
    config.model_context_window = Some(872_000);
    config.model_auto_compact_token_limit = Some(512_000);
    config.update_plan_enabled = true;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_history_recovers_omitted_originals_after_new_context_and_resume() -> TestResult {
    exercise_local_history_recovery(RecoveryHome::Same).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_history_and_notes_survive_resume_in_a_different_home() -> TestResult {
    exercise_local_history_recovery(RecoveryHome::Different).await
}

enum RecoveryHome {
    Same,
    Different,
}

async fn exercise_local_history_recovery(recovery_home: RecoveryHome) -> TestResult {
    let home = Arc::new(tempfile::TempDir::new()?);
    let durable_store = tempfile::TempDir::new()?;
    let local_config = format!(
        "[features.context_management]\nexperimental_mode = true\nbackend = 'local'\nlocal_store_dir = {}\n",
        serde_json::to_string(durable_store.path())?,
    );
    if matches!(recovery_home, RecoveryHome::Different) {
        std::fs::write(home.path().join("config.toml"), &local_config)?;
    }
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(&server, vec![
        response("r1", ev_function_call("bad-plan", "update_plan", &json!({
            "plan": [{"step": "exercise a native tool failure", "status": TOOL_ERROR}]
        }).to_string())),
        response("r2", ev_function_call_with_namespace("save-note", "local_notes", "write_file", &json!({
            "path": "INDEX.md", "text": "Progress: continue the synthetic recovery test."
        }).to_string())),
        response("r3", ev_function_call("roll-over", "new_context", "{}")),
        response("r4", ev_function_call_with_namespace("find-user", "local_history", "search_contents", &json!({
            "query": USER_CONSTRAINT, "role": "user"
        }).to_string())),
        response("r5", ev_function_call_with_namespace("find-error", "local_history", "search_contents", &json!({
            "query": TOOL_ERROR, "role": "tool"
        }).to_string())),
        response("r6", ev_function_call_with_namespace("list-windows", "local_history", "list_windows", "{}")),
        response("r7", ev_function_call_with_namespace("list-items", "local_history", "list_items", "{\"role\":\"user\",\"limit\":1,\"max_chars_per_item\":200}")),
        response("r8", ev_assistant_message("done-1", "recovered")),
    ]).await;
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut registry,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("synthetic")),
    );
    let registry = Arc::new(registry.build());
    let mut builder = test_codex()
        .with_home(home)
        .with_extensions(Arc::clone(&registry))
        .with_config(configure_local);
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn(USER_CONSTRAINT).await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 8);
    assert!(!requests[3].body_contains_text(USER_CONSTRAINT));
    assert!(!requests[3].body_contains_text(TOOL_ERROR));
    assert!(requests[3].body_contains_text("Progress: continue the synthetic recovery test."));
    let user_result = output(&requests, "find-user")?;
    let error_result = output(&requests, "find-error")?;
    assert!(user_result.to_string().contains(USER_CONSTRAINT));
    assert!(error_result.to_string().contains(TOOL_ERROR));
    let original_error = requests[1]
        .function_call_output_text("bad-plan")
        .ok_or("missing original tool error")?;
    assert!(original_error.contains(TOOL_ERROR));
    let user_item = &user_result["items"][0];
    let error_item = error_result["items"]
        .as_array()
        .ok_or("history search items must be an array")?
        .iter()
        .find(|item| item["kind"] == "function_call_output" && item["call_id"] == "bad-plan")
        .ok_or("search must return the actual tool output, not only its call arguments")?;
    assert!(user_item["item_id"].is_string());
    assert!(error_item["item_id"].is_string());
    assert_eq!(user_item["window_id"], error_item["window_id"]);
    let windows = output(&requests, "list-windows")?;
    assert_eq!(
        windows["windows"]
            .as_array()
            .ok_or("missing windows")?
            .len(),
        2
    );
    assert_eq!(windows["windows"][0]["window_id"], user_item["window_id"]);
    let listed = output(&requests, "list-items")?;
    assert_eq!(
        listed["items"]
            .as_array()
            .ok_or("missing listed items")?
            .len(),
        1
    );
    // Native history also contains startup context in the user role.
    assert_eq!(listed["items"][0]["role"], "user");
    assert!(listed["items"][0]["item_id"].is_string());
    assert!(
        listed["items"][0]["truncated_content"]
            .as_str()
            .ok_or("missing listed item content")?
            .chars()
            .count()
            <= 200
    );
    assert!(listed.to_string().len() <= 8000);
    for (namespace, name) in [
        ("local_history", "list_windows"),
        ("local_history", "list_items"),
        ("local_history", "read_item"),
        ("local_history", "search_contents"),
        ("local_notes", "list_files_by_prefix"),
        ("local_notes", "read_file"),
        ("local_notes", "search_contents"),
        ("local_notes", "append_to_file"),
        ("local_notes", "write_file"),
    ] {
        assert!(
            requests[0].tool_by_name(namespace, name).is_some(),
            "missing serialized local tool {namespace}.{name}"
        );
    }
    for (namespace, name) in [
        ("history", "list_windows"),
        ("history", "list_items"),
        ("history", "read_item"),
        ("history", "search_contents"),
        ("notes", "list_files_by_prefix"),
        ("notes", "read_file"),
        ("notes", "search_contents"),
        ("notes", "append_to_file"),
        ("notes", "write_file"),
    ] {
        assert!(
            requests[0].tool_by_name(namespace, name).is_none(),
            "reserved tool alias leaked into local request: {namespace}.{name}"
        );
    }
    let query_schema = requests[0]
        .tool_by_name("local_history", "search_contents")
        .ok_or("missing local history search schema")?;
    assert_eq!(
        query_schema["parameters"]["properties"]["query"].get("encrypted"),
        None
    );
    let read_args = |item: &Value| {
        json!({"window_id": item["window_id"], "item_id": item["item_id"]}).to_string()
    };

    let resume_server = start_mock_server().await;
    let resumed_mock = mount_sse_sequence(
        &resume_server,
        vec![
            response(
                "r7",
                ev_function_call_with_namespace(
                    "read-user",
                    "local_history",
                    "read_item",
                    &read_args(user_item),
                ),
            ),
            response(
                "r8",
                ev_function_call_with_namespace(
                    "read-error",
                    "local_history",
                    "read_item",
                    &read_args(error_item),
                ),
            ),
            response(
                "r9",
                ev_function_call_with_namespace(
                    "read-note",
                    "local_notes",
                    "read_file",
                    "{\"path\":\"INDEX.md\"}",
                ),
            ),
            response("r10", ev_assistant_message("done-2", "resumed")),
        ],
    )
    .await;
    let mut resumed_builder = test_codex()
        .with_extensions(registry)
        .with_config(configure_local);
    let resumed = match recovery_home {
        RecoveryHome::Same => resumed_builder.restart(&resume_server, &test).await?,
        RecoveryHome::Different => {
            let next_home = Arc::new(tempfile::TempDir::new()?);
            std::fs::write(next_home.path().join("config.toml"), &local_config)?;
            let rollout_path = test
                .session_configured
                .rollout_path
                .clone()
                .ok_or("missing rollout path")?;
            test.codex.shutdown_and_wait().await?;
            let resumed = resumed_builder
                .resume(&resume_server, next_home, rollout_path)
                .await?;
            assert_ne!(resumed.config.codex_home, test.config.codex_home);
            assert_eq!(
                resumed.config.context_management_local_store_dir,
                test.config.context_management_local_store_dir
            );
            assert!(
                !resumed
                    .config
                    .codex_home
                    .join("context-management-local")
                    .exists()
            );
            resumed
        }
    };
    resumed
        .submit_turn("Read the returned original references again.")
        .await?;
    let resumed_requests = resumed_mock.requests();
    assert!(
        output(&resumed_requests, "read-user")?
            .to_string()
            .contains(USER_CONSTRAINT)
    );
    assert_eq!(
        output(&resumed_requests, "read-error")?["content"],
        original_error
    );
    let note = output(&resumed_requests, "read-note")?;
    assert!(
        note.to_string()
            .contains("Progress: continue the synthetic recovery test.")
    );
    assert!(!note.to_string().contains(USER_CONSTRAINT));
    assert!(!note.to_string().contains(TOOL_ERROR));
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_keeps_legacy_calls_as_history_without_advertising_reserved_aliases() -> TestResult {
    fn contains_namespaced_call(value: &Value, namespace: &str, name: &str) -> bool {
        match value {
            Value::Array(values) => values
                .iter()
                .any(|value| contains_namespaced_call(value, namespace, name)),
            Value::Object(map) => {
                (map.get("type").and_then(Value::as_str) == Some("function_call")
                    && map.get("namespace").and_then(Value::as_str) == Some(namespace)
                    && map.get("name").and_then(Value::as_str) == Some(name))
                    || map
                        .values()
                        .any(|value| contains_namespaced_call(value, namespace, name))
            }
            _ => false,
        }
    }

    let home = Arc::new(tempfile::TempDir::new()?);
    let server = start_mock_server().await;
    let initial_mock = mount_sse_sequence(
        &server,
        vec![
            response(
                "legacy-1",
                ev_function_call_with_namespace(
                    "legacy-history-call",
                    "history",
                    "list_items",
                    "{}",
                ),
            ),
            response(
                "legacy-2",
                ev_assistant_message("legacy-done", "legacy call recorded"),
            ),
        ],
    )
    .await;
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut registry,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("synthetic")),
    );
    let registry = Arc::new(registry.build());
    let test = test_codex()
        .with_home(home)
        .with_extensions(Arc::clone(&registry))
        .with_config(configure_local)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Record one legacy local-tool call.")
        .await?;
    assert_eq!(initial_mock.requests().len(), 2);

    let resume_server = start_mock_server().await;
    let resumed_mock = mount_sse_sequence(
        &resume_server,
        vec![response(
            "legacy-3",
            ev_assistant_message("resume-done", "resume complete"),
        )],
    )
    .await;
    let resumed = test_codex()
        .with_extensions(registry)
        .with_config(configure_local)
        .restart(&resume_server, &test)
        .await?;
    resumed.submit_turn("Continue after resume.").await?;
    let requests = resumed_mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(contains_namespaced_call(
        &request.body_json()["input"],
        "history",
        "list_items"
    ));
    assert!(request.tool_by_name("history", "list_items").is_none());
    assert!(request.tool_by_name("notes", "write_file").is_none());
    assert!(
        request
            .tool_by_name("local_history", "list_items")
            .is_some()
    );
    assert!(request.tool_by_name("local_notes", "write_file").is_some());
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_history_survives_automatic_budget_rollover() -> TestResult {
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("auto-1"),
                ev_function_call("budget-trigger", "get_context_remaining", "{}"),
                ev_completed_with_tokens("auto-1", /*total_tokens*/ 9_500),
            ]),
            sse(vec![
                ev_response_created("auto-2"),
                ev_function_call("budget-buffer", "get_context_remaining", "{}"),
                ev_completed_with_tokens("auto-2", /*total_tokens*/ 13_500),
            ]),
            response(
                "auto-3",
                ev_function_call_with_namespace(
                    "find-after-auto",
                    "local_history",
                    "search_contents",
                    &json!({
                        "query": USER_CONSTRAINT, "role": "user"
                    })
                    .to_string(),
                ),
            ),
            response(
                "auto-4",
                ev_assistant_message("auto-done", "recovered after automatic rollover"),
            ),
        ],
    )
    .await;
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut registry,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("synthetic")),
    );
    let test = test_codex()
        .with_extensions(Arc::new(registry.build()))
        .with_config(|config| {
            configure_local(config);
            config.model_provider.name = "OpenAI (test)".into();
            config.model_context_window = Some(50_000);
            config.model_auto_compact_token_limit = Some(9_000);
            config.token_budget = Some(TokenBudgetConfig {
                auto_compact_fallback_prompt: Some(
                    "Write notes and start a new context.".to_string(),
                ),
                auto_compact_fallback_buffer_tokens: Some(4_000),
                ..TokenBudgetConfig::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(USER_CONSTRAINT).await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[2].body_contains_text(USER_CONSTRAINT));
    assert!(
        output(&requests, "find-after-auto")?
            .to_string()
            .contains(USER_CONSTRAINT)
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
