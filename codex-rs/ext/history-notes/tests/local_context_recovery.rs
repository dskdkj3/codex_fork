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
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(&server, vec![
        response("r1", ev_function_call("bad-plan", "update_plan", &json!({
            "plan": [{"step": "exercise a native tool failure", "status": TOOL_ERROR}]
        }).to_string())),
        response("r2", ev_function_call_with_namespace("save-note", "notes", "write_file", &json!({
            "path": "INDEX.md", "text": "Progress: continue the synthetic recovery test."
        }).to_string())),
        response("r3", ev_function_call("roll-over", "new_context", "{}")),
        response("r4", ev_function_call_with_namespace("find-user", "history", "search_contents", &json!({
            "query": USER_CONSTRAINT, "role": "user"
        }).to_string())),
        response("r5", ev_function_call_with_namespace("find-error", "history", "search_contents", &json!({
            "query": TOOL_ERROR, "role": "tool"
        }).to_string())),
        response("r6", ev_function_call_with_namespace("list-windows", "history", "list_windows", "{}")),
        response("r7", ev_function_call_with_namespace("list-items", "history", "list_items", "{\"role\":\"user\",\"limit\":1,\"max_chars_per_item\":200}")),
        response("r8", ev_assistant_message("done-1", "recovered")),
    ]).await;
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut registry,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("synthetic")),
    );
    let registry = Arc::new(registry.build());
    let mut builder = test_codex()
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
    let original_error = requests[1].function_call_output_text("bad-plan").unwrap();
    assert!(original_error.contains(TOOL_ERROR));
    let user_item = &user_result["items"][0];
    let error_item = error_result["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "function_call_output" && item["call_id"] == "bad-plan")
        .expect("search must return the actual tool output, not only its call arguments");
    assert!(user_item["item_id"].is_string());
    assert!(error_item["item_id"].is_string());
    assert_eq!(user_item["window_id"], error_item["window_id"]);
    let windows = output(&requests, "list-windows")?;
    assert_eq!(windows["windows"].as_array().unwrap().len(), 2);
    assert_eq!(windows["windows"][0]["window_id"], user_item["window_id"]);
    let listed = output(&requests, "list-items")?;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    // Native history also contains startup context in the user role.
    assert_eq!(listed["items"][0]["role"], "user");
    assert!(listed["items"][0]["item_id"].is_string());
    assert!(
        listed["items"][0]["truncated_content"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 200
    );
    assert!(listed.to_string().len() <= 8000);
    let query_schema = requests[0]
        .tool_by_name("history", "search_contents")
        .unwrap();
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
                    "history",
                    "read_item",
                    &read_args(user_item),
                ),
            ),
            response(
                "r8",
                ev_function_call_with_namespace(
                    "read-error",
                    "history",
                    "read_item",
                    &read_args(error_item),
                ),
            ),
            response(
                "r9",
                ev_function_call_with_namespace(
                    "read-note",
                    "notes",
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
    let resumed = resumed_builder
        .restart(&resume_server, &test)
        .await
        .expect("resume local context thread");
    resumed
        .submit_turn("Read the returned original references again.")
        .await
        .expect("submit resumed recovery turn");
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
                    "history",
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
