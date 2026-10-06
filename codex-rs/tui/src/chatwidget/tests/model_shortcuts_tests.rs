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

#[tokio::test]
async fn recent_model_shortcut_works_before_planning_and_in_the_plan_handoff() {
    let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
    let (mut chat, mut events, _ops) = make_chatwidget_manual(Some("gpt-5.5")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_collaboration_mask(
        collaboration_modes::plan_mask(chat.model_catalog.as_ref()).unwrap(),
    );
    chat.bottom_pane
        .set_composer_text("Keep this draft".into(), Vec::new(), Vec::new());
    while events.try_recv().is_ok() {}

    chat.handle_key_event(key);
    assert_matches!(events.try_recv(), Ok(AppEvent::ToggleRecentModel));
    chat.set_model("gpt-5.6-terra");
    assert_eq!(chat.current_model(), "gpt-5.6-terra");
    assert_eq!(chat.active_mode_kind(), ModeKind::Plan);

    chat.on_plan_item_completed("- Implement the change".into());
    chat.open_plan_implementation_prompt();
    chat.handle_key_event(KeyCode::Down.into());
    while events.try_recv().is_ok() {}

    chat.handle_key_event(key);
    assert_matches!(events.try_recv(), Ok(AppEvent::ToggleRecentModel));
    chat.set_model("gpt-5.5");
    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert!(popup.contains(chat.model_display_name()), "{popup}");
    assert!(popup.contains("› 2."), "{popup}");
    assert_eq!(chat.active_mode_kind(), ModeKind::Plan);
    assert_eq!(chat.bottom_pane.composer_text(), "Keep this draft");

    chat.handle_key_event(KeyCode::Enter.into());
    assert_matches!(events.try_recv(), Ok(AppEvent::ClearUiAndSubmitUserMessage { text })
        if text.ends_with("- Implement the change"));
    assert!(chat.bottom_pane.no_modal_or_popup_active());

    let mut keymap = RuntimeKeymap::defaults();
    keymap.list.move_down = vec![key_hint::alt(KeyCode::Char('m'))];
    chat.bottom_pane.set_keymap_bindings(&keymap);
    chat.open_plan_implementation_prompt();
    while events.try_recv().is_ok() {}
    chat.handle_key_event(key);
    assert!(events.try_recv().is_err());
    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert!(popup.contains("› 2."), "{popup}");
}
