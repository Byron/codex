//! Model switching uses the same main-surface input ownership as reasoning shortcuts.

use super::ChatWidget;
use super::PARENT_OWNED_INPUT_MESSAGE;
use crate::app_event::AppEvent;
use crate::key_hint::KeyBindingListExt;
use crossterm::event::KeyEvent;

impl ChatWidget {
    pub(super) fn handle_model_shortcut(&mut self, key_event: KeyEvent) -> bool {
        if !self.chat_keymap.toggle_recent_model.is_pressed(key_event)
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return false;
        }
        if self.blocks_direct_input {
            self.add_error_message(PARENT_OWNED_INPUT_MESSAGE.to_string());
        } else if !self.is_session_configured() {
            self.add_info_message(
                "Model switching is disabled until startup completes.".to_string(),
                /*hint*/ None,
            );
        } else if self.restrict_model_picker_to_luna_reserve() {
            self.add_info_message(
                "Model switching is unavailable until ordinary usage recovers.".to_string(),
                /*hint*/ None,
            );
        } else {
            self.app_event_tx.send(AppEvent::ToggleRecentModel);
        }
        true
    }
}
