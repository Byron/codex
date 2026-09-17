use super::*;
use crate::app_event_sender::AppEventSender;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::edit::ConfigEditsBuilder;
use crate::motion::MotionMode;
use crate::render::renderable::Renderable;
use crate::status_indicator_widget::StatusIndicatorWidget;
use crate::status_indicator_widget::StatusTimer;
use crate::tui::FrameRequester;
use codex_config::LoaderOverrides;
use codex_config::types::SessionPickerViewMode;
use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc::unbounded_channel;

#[tokio::test]
async fn explicit_animations_override_accessibility_defaults() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut snapshots = Vec::new();
    for (configured, cli_override, expected_animated, expected_reduced) in [
        (None, None, true, false),
        (Some(true), None, true, true),
        (Some(false), None, false, false),
        (Some(false), Some(true), true, true),
        (Some(true), Some(false), false, false),
    ] {
        let mut config_text = "[tui]\nwhimsy = true\n".to_string();
        if let Some(configured) = configured {
            config_text.push_str(&format!("animations = {configured}\n"));
        }
        std::fs::write(home.path().join("config.toml"), &config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .cli_overrides(
                cli_override
                    .map(|enabled| ("tui.animations".into(), toml::Value::Boolean(enabled)))
                    .into_iter()
                    .collect(),
            )
            .build()
            .await?;
        let animated = LocalSettings::with_accessibility_preferences(
            &config,
            MotionMode::Animated,
            MotionMode::Animated,
        );
        for (system_motion, screen_reader_default, animations) in [
            (
                MotionMode::Animated,
                MotionMode::Animated,
                expected_animated,
            ),
            (MotionMode::Reduced, MotionMode::Animated, expected_reduced),
            (MotionMode::Animated, MotionMode::Reduced, expected_reduced),
            (MotionMode::Reduced, MotionMode::Reduced, expected_reduced),
        ] {
            let local = LocalSettings::with_accessibility_preferences(
                &config,
                system_motion,
                screen_reader_default,
            );
            let mut expected = animated.clone();
            expected.tui.animations = animations;
            assert_eq!(local, expected);

            let (tx, _rx) = unbounded_channel();
            let row = StatusIndicatorWidget::new(
                AppEventSender::new(tx),
                FrameRequester::test_dummy(),
                local.tui.animations,
            );
            let mut timer = StatusTimer::default();
            timer.pause_at(std::time::Instant::now());
            let mut terminal =
                Terminal::new(TestBackend::new(/*width*/ 40, /*height*/ 1))?;
            terminal.draw(|frame| {
                row.with_timer(&timer)
                    .render(frame.area(), frame.buffer_mut())
            })?;
            snapshots.push(format!(
                "config {configured:?}, CLI {cli_override:?}, system {system_motion:?}, screen reader {screen_reader_default:?}:\n{}",
                terminal.backend(),
            ));
        }
        assert_eq!(config.animations, expected_animated);
        assert_eq!(
            std::fs::read_to_string(home.path().join("config.toml"))?,
            config_text
        );
    }
    insta::assert_snapshot!("animation_preference_status_rows", snapshots.join("\n"));
    Ok(())
}

#[tokio::test]
async fn local_load_preserves_defaults_and_resolved_overrides() -> anyhow::Result<()> {
    for config_text in [
        "",
        r#"
[tui]
animations = false
whimsy = false
show_tooltips = false
show_server_version_notice = false
auto_recap = false
vim_mode_default = true
terminal_resize_reflow_max_rows = 0
session_picker_view = "comfortable"
[history]
persistence = "none"
max_bytes = 4096
[notice]
fast_default_opt_out = true
"#,
    ] {
        let home = tempfile::tempdir()?;
        std::fs::write(home.path().join("config.toml"), config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .cli_overrides(vec![("tui.disable_paste_burst".into(), true.into())])
            .build()
            .await?;
        let local = LocalSettings::from(&config);
        let mut expected: Tui = toml::from_str("")?;
        expected.disable_paste_burst = Some(true);
        expected.session_picker_view = Some(SessionPickerViewMode::Dense);
        if !config_text.is_empty() {
            expected.animations = false;
            expected.whimsy = false;
            expected.show_tooltips = false;
            expected.show_server_version_notice = false;
            expected.auto_recap = false;
            expected.vim_mode_default = true;
            expected.terminal_resize_reflow_max_rows = Some(0);
            expected.session_picker_view = Some(SessionPickerViewMode::Comfortable);
        }
        assert_eq!(local.tui, expected);
        assert_eq!(
            local.terminal_resize_reflow(),
            config.terminal_resize_reflow
        );
        assert_eq!(
            (&local.history, &local.notices),
            (&config.history, &config.notices)
        );
    }
    Ok(())
}

#[tokio::test]
async fn local_writes_preserve_selected_user_file_and_home_destinations() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AbsolutePathBuf::from_absolute_path(home.path().join("work.config.toml"))?;
    std::fs::write(&selected, "[tui]\ntheme = \"dracula\"\n")?;
    let overrides = LoaderOverrides {
        user_config_path: Some(selected.clone()),
        user_config_profile: Some("work".parse()?),
        ignore_project_config: true,
        ..LoaderOverrides::without_managed_config_for_tests()
    };
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(overrides.clone())
        .build()
        .await?;
    let local = LocalSettings::from(&config);
    assert_eq!(local.user_config_path, selected);
    ConfigEditsBuilder::for_config_path(local.user_config_path.as_path())
        .with_edits([crate::legacy_core::config::edit::syntax_theme_edit("nord")])
        .apply()
        .await?;
    ConfigEditsBuilder::new(local.codex_home.as_path())
        .set_session_picker_view(SessionPickerViewMode::Comfortable)
        .apply()
        .await?;
    let reloaded = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(overrides)
        .build()
        .await?;
    assert_eq!(
        LocalSettings::from(&reloaded).tui.theme.as_deref(),
        Some("nord")
    );
    let home_config: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.path().join("config.toml"))?)?;
    assert_eq!(
        home_config["tui"]["session_picker_view"].as_str(),
        Some("comfortable")
    );
    assert_eq!(home_config["tui"].get("theme"), None);
    Ok(())
}

#[tokio::test]
async fn screen_reader_default_yields_to_preferences_on_reload() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    for (config_text, expected) in [
        ("", false),
        ("[tui]\nanimations = true\n", true),
        ("[tui]\nanimations = false\n", false),
    ] {
        std::fs::write(home.path().join("config.toml"), config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let local = LocalSettings::with_accessibility_preferences(
            &config,
            MotionMode::Animated,
            MotionMode::Reduced,
        );
        assert_eq!(local.tui.animations, expected);
    }
    Ok(())
}
