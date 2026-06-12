use serde::Serialize;

use kairos_config_derive::ConfigDeserialize;
use kairos_terminal::term::SEMANTIC_ESCAPE_CHARS;

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub semantic_escape_chars: String,
    pub save_to_clipboard: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(),
            // Windows and macOS have no primary ("selection") clipboard, so a plain selection
            // would otherwise never reach the system clipboard. Copy it there on select so
            // dragging to select (see the left-button handling in `input`) also copies, which is
            // what most apps that grab the mouse — e.g. Claude Code — used to do themselves.
            save_to_clipboard: cfg!(any(windows, target_os = "macos")),
        }
    }
}
