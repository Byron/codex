use super::*;
use crate::collaboration_modes;
use codex_app_server_client::AppServerClient;
use codex_config::LoaderOverrides;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn session_model_selection_preserves_defaults_and_updates_active_thread() -> Result<()> {
    for mode in [ModeKind::Default, ModeKind::Plan] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        app.config.model = Some("gpt-5.5".into());
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Medium);
        app.config.plan_mode_reasoning_effort = Some(ReasoningEffortConfig::Low);
        let config_path = app.config.codex_home.join("config.toml");
        let original = "# My defaults\nmodel = 'gpt-5.5'\nmodel_reasoning_effort = 'medium'\nplan_mode_reasoning_effort = 'low'\n";
        std::fs::write(&config_path, original)?;
        let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
        let started = server.start_thread(&app.config).await?;
        let thread_id = started.session.thread_id;
        app.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        app.chat_widget
            .set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
        if mode == ModeKind::Plan {
            let mask = collaboration_modes::plan_mask(app.model_catalog.as_ref()).unwrap();
            app.chat_widget.set_collaboration_mask(mask);
        }
        let mut tui = crate::tui::test_support::make_test_tui()?;
        for effort in [
            ReasoningEffortConfig::High,
            ReasoningEffortConfig::Ultra,
            ReasoningEffortConfig::Max,
        ] {
            while events.try_recv().is_ok() {}
            Box::pin(app.handle_event(
                &mut tui,
                &mut server,
                AppEvent::SelectSessionModel {
                    model: "gpt-5.6-terra".into(),
                    effort: Some(effort.clone()),
                },
            ))
            .await?;
            let settings = next_thread_settings_updated(&mut server, thread_id).await;
            assert_eq!(
                (
                    settings.thread_settings.model.as_str(),
                    settings.thread_settings.effort.clone()
                ),
                ("gpt-5.6-terra", Some(effort.clone()))
            );
            if mode == ModeKind::Default && effort == ReasoningEffortConfig::High {
                insta::assert_snapshot!(
                    "session_only_model_confirmation",
                    next_history_message(&mut events)
                );
            }
            app.enqueue_thread_notification(
                thread_id,
                ServerNotification::ThreadSettingsUpdated(settings),
            )
            .await?;
            assert_eq!(
                (
                    app.chat_widget.current_model(),
                    app.chat_widget.current_reasoning_effort()
                ),
                ("gpt-5.6-terra", Some(effort))
            );
            assert_eq!(std::fs::read(&config_path)?, original.as_bytes());
        }
        let fresh = server.start_thread(&app.config).await?;
        assert_eq!(
            (fresh.session.model.as_str(), fresh.session.reasoning_effort),
            ("gpt-5.5", Some(ReasoningEffortConfig::Medium))
        );
        assert_eq!(
            app.config.plan_mode_reasoning_effort,
            Some(ReasoningEffortConfig::Low)
        );
        server.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn model_default_saves_report_server_outcomes_and_target_server_profile() -> Result<()> {
    for outcome in ["saved", "overridden", "rejected"] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Medium);
        let local_config_path = app.config.codex_home.join("config.toml");
        let local_config_before = std::fs::read(&local_config_path).ok();
        let server_home = tempfile::tempdir()?;
        let base_path = server_home.path().join("config.toml");
        let profile_path = server_home.path().join("work.config.toml");
        std::fs::write(&base_path, "# Base configuration stays unchanged.\n")?;
        std::fs::write(&profile_path, "")?;
        let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
        loader_overrides.user_config_path =
            Some(AbsolutePathBuf::from_absolute_path(&profile_path)?);
        loader_overrides.user_config_profile = Some("work".parse()?);
        let overrides = if outcome == "overridden" {
            vec![
                ("model".into(), toml::Value::String("gpt-5.6-terra".into())),
                (
                    "model_reasoning_effort".into(),
                    toml::Value::String("low".into()),
                ),
                (
                    "plan_mode_reasoning_effort".into(),
                    toml::Value::String("low".into()),
                ),
                ("service_tier".into(), toml::Value::String("flex".into())),
            ]
        } else {
            Vec::new()
        };
        let config = ConfigBuilder::default()
            .codex_home(server_home.path().to_path_buf())
            .cli_overrides(overrides.clone())
            .loader_overrides(loader_overrides.clone())
            .harness_overrides(ConfigOverrides {
                cwd: Some(server_home.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await?;
        let client = crate::start_embedded_app_server(
            codex_arg0::Arg0DispatchPaths::default(),
            config,
            overrides,
            loader_overrides,
            /*strict_config*/ false,
            codex_config::CloudConfigBundleLoader::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            /*state_db*/ None,
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            Default::default(),
        )
        .await?;
        let mut server = AppServerSession::new(
            AppServerClient::InProcess(client),
            crate::app_server_session::ThreadParamsMode::Remote,
        );
        if outcome == "rejected" {
            std::fs::write(&profile_path, "[broken")?;
        }
        while events.try_recv().is_ok() {}
        let mut tui = crate::tui::test_support::make_test_tui()?;
        for event in [
            AppEvent::PersistModelSelection {
                model: "gpt-5.5".into(),
                effort: Some(ReasoningEffortConfig::High),
            },
            AppEvent::PersistPlanModeReasoningEffort(Some(ReasoningEffortConfig::High)),
            AppEvent::PersistServiceTierSelection {
                service_tier: Some(ServiceTier::Fast.request_value().into()),
            },
            AppEvent::ApplyAdvancedReasoning {
                model: "gpt-5.5".into(),
                effort: ReasoningEffortConfig::Ultra,
            },
        ] {
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
        }
        let mut service_tier_error = None;
        let messages = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => {
                    if outcome == "rejected" {
                        let text = lines_to_single_string(&cell.transcript_lines(/*width*/ 200));
                        if text.contains("Failed to save default service tier") {
                            service_tier_error = Some(
                                text.replace(&profile_path.display().to_string(), "<config-path>"),
                            );
                        }
                    }
                    Some(lines_to_single_string(&cell.transcript_lines(/*width*/ 80)))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            messages.matches("higher-priority").count(),
            if outcome == "overridden" { 4 } else { 0 }
        );
        assert_eq!(
            messages.matches("Failed to save").count(),
            if outcome == "rejected" { 4 } else { 0 }
        );
        if outcome == "overridden" {
            insta::assert_snapshot!("overridden_model_defaults", messages);
        }
        let persisted = std::fs::read_to_string(&profile_path)?;
        if outcome == "rejected" {
            insta::assert_snapshot!(
                "service_tier_default_save_error",
                service_tier_error.expect("expected a service-tier save error")
            );
            assert_eq!(persisted, "[broken");
            assert!(!messages.contains("Model changed"));
            assert!(!messages.contains("Service tier set"));
        } else {
            assert_eq!(
                toml::from_str::<toml::Value>(&persisted)?,
                toml::Value::Table(toml::toml! {
                    model = "gpt-5.5"
                    model_reasoning_effort = "medium"
                    plan_mode_reasoning_effort = "high"
                    service_tier = "fast"
                })
            );
        }
        assert_eq!(std::fs::read(&local_config_path).ok(), local_config_before);
        assert_eq!(
            std::fs::read_to_string(&base_path)?,
            "# Base configuration stays unchanged.\n"
        );
        server.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn recent_model_shortcut_restores_choices_across_restarts_without_saving_defaults()
-> Result<()> {
    for mode in [ModeKind::Default, ModeKind::Plan] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        app.config.model = Some("gpt-5.5".into());
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::High);
        app.config.plan_mode_reasoning_effort = Some(ReasoningEffortConfig::High);
        let config_path = app.local_settings.codex_home.join("config.toml");
        let original = "model = 'gpt-5.5'\nmodel_reasoning_effort = 'high'\n";
        std::fs::write(&config_path, original)?;
        let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
        let started = server.start_thread(&app.config).await?;
        let thread_id = started.session.thread_id;
        app.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        app.chat_widget
            .set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
        if mode == ModeKind::Plan {
            let mask = collaboration_modes::plan_mask(app.model_catalog.as_ref()).unwrap();
            app.chat_widget.set_collaboration_mask(mask);
            app.chat_widget
                .set_plan_mode_reasoning_effort(Some(ReasoningEffortConfig::High));
        }
        for preset in &mut Arc::make_mut(&mut app.model_catalog).models {
            if ["gpt-6.1-sol", "gpt-5.5", "gpt-5.6-terra"].contains(&preset.model.as_str()) {
                preset.show_in_picker = true;
            }
        }
        let preset = Arc::make_mut(&mut app.model_catalog)
            .models
            .iter_mut()
            .find(|preset| preset.model == "gpt-5.6-terra")
            .unwrap();
        preset.supported_reasoning_efforts.push(
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffortConfig::Ultra,
                description: "Ultra".into(),
            },
        );
        let mut tui = crate::tui::test_support::make_test_tui()?;
        while events.try_recv().is_ok() {}
        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::ToggleRecentModel)).await?;
        if mode == ModeKind::Default {
            insta::assert_snapshot!(
                "recent_model_missing_alternative",
                next_history_message(&mut events)
            );
        }
        for (manual, model, effort) in [
            (true, "gpt-5.6-terra", ReasoningEffortConfig::Ultra),
            (false, "gpt-5.5", ReasoningEffortConfig::High),
            (false, "gpt-5.6-terra", ReasoningEffortConfig::Ultra),
            (true, "gpt-6.1-sol", ReasoningEffortConfig::Low),
            (false, "gpt-5.6-terra", ReasoningEffortConfig::Ultra),
            (false, "gpt-6.1-sol", ReasoningEffortConfig::Low),
        ] {
            while events.try_recv().is_ok() {}
            let event = if manual {
                AppEvent::SelectSessionModel {
                    model: model.into(),
                    effort: Some(effort.clone()),
                }
            } else {
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT));
                let event = events.try_recv().expect("model shortcut event");
                assert_matches!(event, AppEvent::ToggleRecentModel);
                event
            };
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
            let settings = next_thread_settings_updated(&mut server, thread_id).await;
            assert_eq!(
                (
                    settings.thread_settings.model.as_str(),
                    settings.thread_settings.effort.clone(),
                    settings.thread_settings.collaboration_mode.mode
                ),
                (model, Some(effort.clone()), mode),
            );
            app.enqueue_thread_notification(
                thread_id,
                ServerNotification::ThreadSettingsUpdated(settings),
            )
            .await?;
            assert_eq!(
                (
                    app.chat_widget.current_model(),
                    app.chat_widget.current_reasoning_effort()
                ),
                (model, Some(effort)),
            );
            assert_eq!(std::fs::read(&config_path)?, original.as_bytes());
            if !manual && model == "gpt-5.5" && mode == ModeKind::Default {
                insta::assert_snapshot!(
                    "recent_model_confirmation",
                    next_history_message(&mut events)
                );
            }
            // Reload the on-disk pair before every toggle, as a new client would.
            app.recent_models = crate::app::recent_models::RecentModels::load(
                app.local_settings.codex_home.as_path(),
            );
        }
        let saved =
            crate::app::recent_models::RecentModels::load(app.local_settings.codex_home.as_path());
        let fallback = app.chat_widget.effective_collaboration_mode().with_updates(
            Some("untracked-fallback".into()),
            /*effort*/ None,
            /*developer_instructions*/ None,
        );
        app.chat_widget.finish_backend_banner_fallback(fallback);
        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::ToggleRecentModel)).await?;
        assert_eq!(app.chat_widget.current_model(), "gpt-6.1-sol");
        assert_eq!(app.recent_models, saved);
        let preset = Arc::make_mut(&mut app.model_catalog)
            .models
            .iter_mut()
            .find(|preset| preset.model == "gpt-5.6-terra")
            .unwrap();
        preset
            .supported_reasoning_efforts
            .retain(|option| option.effort != ReasoningEffortConfig::Ultra);
        let fallback_effort = preset.default_reasoning_effort.clone();
        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::ToggleRecentModel)).await?;
        assert_eq!(
            app.chat_widget.current_reasoning_effort(),
            Some(fallback_effort)
        );
        Arc::make_mut(&mut app.model_catalog)
            .models
            .retain(|preset| preset.model != "gpt-6.1-sol");
        while events.try_recv().is_ok() {}
        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::ToggleRecentModel)).await?;
        assert_eq!(app.chat_widget.current_model(), "gpt-5.6-terra");
        if mode == ModeKind::Default {
            insta::assert_snapshot!(
                "recent_model_unavailable",
                next_history_message(&mut events)
            );
        }
        server.shutdown().await?;
    }
    Ok(())
}
