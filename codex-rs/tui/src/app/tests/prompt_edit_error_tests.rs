use super::*;
use pretty_assertions::assert_eq;
use session_lifecycle_requests::HistoryCapabilities;
use session_lifecycle_requests::start_recording_app_server_with_history;

#[tokio::test]
async fn prompt_edit_fatal_error_displays_server_reason_and_restores_prompt() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let config = app.chat_widget.config_ref().clone();
    let session_id = app_test_support::create_fake_paginated_rollout(
        &config.codex_home,
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "edit this prompt",
        Some("test-provider"),
        /*git_info*/ None,
    )
    .expect("materialized rollout should be created");
    let thread_id = ThreadId::from_string(&session_id)?;
    let source_path =
        app_test_support::rollout_path(&config.codex_home, "2025-01-05T12-00-00", &session_id);
    let metadata = std::fs::read_to_string(&source_path)?
        .lines()
        .next()
        .expect("rollout metadata")
        .to_string();
    std::fs::write(&source_path, format!("{metadata}\n"))?;
    for item in [
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "completed-turn".into(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::default(),
        })),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(
            codex_protocol::protocol::ItemCompletedEvent {
                thread_id,
                turn_id: "completed-turn".into(),
                item: codex_protocol::items::TurnItem::UserMessage(
                    codex_protocol::items::UserMessageItem {
                        id: "selected-prompt".into(),
                        client_id: None,
                        content: vec![codex_protocol::user_input::UserInput::Text {
                            text: "edit this prompt".into(),
                            text_elements: Vec::new(),
                        }],
                    },
                ),
                started_at_ms: None,
                completed_at_ms: 0,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "completed-turn".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ] {
        codex_rollout::append_rollout_item_to_path(&source_path, &item).await?;
    }
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &config,
        HistoryCapabilities::RevertFails,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        LoaderOverrides::default(),
    )
    .await?;
    let started = app_server
        .resume_thread(
            &app.local_settings,
            config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::OverrideFromCurrentConfig,
        )
        .await?;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    while let Ok(event) = app_event_rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            app.transcript_cells.push(Arc::from(cell));
        }
    }
    let selected_cell = Arc::clone(
        &app.transcript_cells[nth_user_position(&app.transcript_cells, /*nth*/ 0).unwrap()],
    );
    let before = app_server
        .thread_read(thread_id, /*include_turns*/ true)
        .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let result = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RevertSessionForPromptEdit {
            thread_id,
            selected_cell,
            prompt: "edit this prompt".into(),
        },
    ))
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("an unconfirmed revert must stop the TUI"),
    };
    // The CLI prints Display, which must include the cause without alternate formatting.
    insta::assert_snapshot!(error.to_string(), @"prompt edit could not be confirmed; resume this session to reload its history: thread/revert failed: forced history persistence failure (code -32603)");
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "edit this prompt"
    );
    assert_eq!(
        app_server
            .thread_read(thread_id, /*include_turns*/ true)
            .await?
            .turns,
        before.turns
    );
    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .filter(|request| request.method == "thread/revert")
            .count(),
        1
    );
    proxy.abort();
    Ok(())
}
