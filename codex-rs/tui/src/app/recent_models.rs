//! Client-local model choices, independent of durable defaults and automatic fallbacks.
//! Two slots bound the history; each model retains its last effective reasoning effort.

use super::App;
use crate::app_server_session::AppServerSession;
use crate::model_catalog::LUNA_RESERVE_MODEL;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::io::Read;
use std::path::Path;

const HISTORY_FILE: &str = "tui-recent-models.json";

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct RecentModels {
    entries: [Option<RecentModel>; 2],
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RecentModel {
    model: String,
    effort: Option<ReasoningEffort>,
}

impl RecentModels {
    pub(super) fn load(codex_home: &Path) -> Self {
        std::fs::File::open(codex_home.join(HISTORY_FILE))
            .ok()
            .and_then(|file| serde_json::from_reader(file.take(/*limit*/ 4096)).ok())
            .unwrap_or_default()
    }

    fn save(&self, codex_home: &Path) -> io::Result<()> {
        std::fs::create_dir_all(codex_home)?;
        let mut file = tempfile::NamedTempFile::new_in(codex_home)?;
        serde_json::to_writer(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(codex_home.join(HISTORY_FILE))
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn remember(&mut self, model: &str, effort: Option<ReasoningEffort>) -> bool {
        let choice = RecentModel {
            model: model.to_string(),
            effort,
        };
        if self.entries[0].as_ref() == Some(&choice) {
            return false;
        }
        if self.entries[0]
            .as_ref()
            .is_none_or(|last| last.model != model)
        {
            self.entries[1] = self.entries[0].take();
        }
        self.entries[0] = Some(choice);
        true
    }
}

impl App {
    pub(super) fn remember_current_model(&mut self) {
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return;
        };
        if self.agent_navigation.is_parent_owned(thread_id)
            || self.chat_widget.is_external_writer_view()
            || self.chat_widget.current_model() == LUNA_RESERVE_MODEL
        {
            return;
        }
        if self.recent_models.remember(
            self.chat_widget.current_model(),
            self.chat_widget.current_reasoning_effort(),
        ) && let Err(error) = self
            .recent_models
            .save(self.local_settings.codex_home.as_path())
        {
            tracing::warn!(%error, "failed to save recent model choices");
            self.chat_widget
                .add_warning_message(format!("Could not save recent model choices: {error}"));
        }
    }

    pub(super) fn remember_current_model_if_known(&mut self) {
        // Capture changes in effective Plan reasoning without adding an automatic fallback.
        if self
            .recent_models
            .entries
            .iter()
            .flatten()
            .any(|choice| choice.model == self.chat_widget.current_model())
        {
            self.remember_current_model();
        }
    }

    pub(super) async fn toggle_recent_model(&mut self, app_server: &mut AppServerSession) {
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return;
        };
        if self.agent_navigation.is_parent_owned(thread_id)
            || self.chat_widget.is_external_writer_view()
        {
            return;
        }
        self.remember_current_model_if_known();
        let Some(choice) = self
            .recent_models
            .entries
            .iter()
            .flatten()
            .find(|choice| choice.model != self.chat_widget.current_model())
            .cloned()
        else {
            self.chat_widget.add_info_message(
                "Choose a second model with /model to switch between recent models.".to_string(),
                /*hint*/ None,
            );
            return;
        };
        let Some(preset) = self
            .model_catalog
            .try_list_models()
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|model| model.show_in_picker && model.model == choice.model)
            })
        else {
            self.chat_widget.add_info_message(
                format!(
                    "Model {} is unavailable. Choose a model with /model.",
                    choice.model
                ),
                /*hint*/ None,
            );
            return;
        };
        let effort = choice
            .effort
            .filter(|effort| {
                preset
                    .supported_reasoning_efforts
                    .iter()
                    .any(|option| &option.effort == effort)
            })
            .unwrap_or(preset.default_reasoning_effort);
        self.select_session_model(app_server, choice.model.clone(), Some(effort))
            .await;
        if self.chat_widget.current_model() == choice.model {
            self.remember_current_model();
        }
    }
}

#[cfg(test)]
#[path = "recent_models_tests.rs"]
mod tests;
