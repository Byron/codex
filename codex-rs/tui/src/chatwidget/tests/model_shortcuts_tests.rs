use super::*;

#[tokio::test]
async fn recent_model_shortcut_preserves_draft_and_obeys_input_ownership() {
    let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
    let (mut chat, mut events, _ops) = make_chatwidget_manual(Some("gpt-5.5")).await;
    chat.bottom_pane
        .set_composer_text("Keep this draft".into(), Vec::new(), Vec::new());
    chat.thread_id = None;
    chat.handle_key_event(key);
    assert_chatwidget_snapshot!(
        "recent_model_shortcut_before_startup",
        lines_to_single_string(&drain_insert_history(&mut events).concat()),
    );
    chat.thread_id = Some(ThreadId::new());
    chat.handle_key_event(key);
    assert_matches!(events.try_recv(), Ok(AppEvent::ToggleRecentModel));
    assert_eq!(chat.bottom_pane.composer_text(), "Keep this draft");
    chat.open_model_popup();
    while events.try_recv().is_ok() {}
    chat.handle_key_event(key);
    assert!(events.try_recv().is_err());
    chat.handle_key_event(KeyEvent::from(KeyCode::Esc));
    while events.try_recv().is_ok() {}
    chat.set_model(crate::model_catalog::LUNA_RESERVE_MODEL);
    chat.handle_key_event(key);
    assert_chatwidget_snapshot!(
        "recent_model_shortcut_usage_exhausted",
        lines_to_single_string(&drain_insert_history(&mut events).concat()),
    );
    chat.set_parent_owned_thread();
    chat.handle_key_event(key);
    assert_chatwidget_snapshot!(
        "recent_model_shortcut_parent_owned",
        lines_to_single_string(&drain_insert_history(&mut events).concat()),
    );
}
