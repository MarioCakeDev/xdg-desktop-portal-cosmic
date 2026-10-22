// SPDX-License-Identifier: GPL-3.0-only

use serde::{Deserialize, Serialize};

/// Logical coordinates of a rectangle selection
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    pub save_location: ImageSaveLocation,
    pub choice: Choice,
    #[serde(default)]
    pub last_rectangle: Option<Rect>,
    /// The target language last picked in the translate toolbar, as a
    /// `lang::LANGS` code (`de`, `ja`, …). `None` means it has never been
    /// picked: the dialog then opens on the session locale, as before.
    ///
    /// Written when the picker changes, not when a translation succeeds, so an
    /// aborted or failed translate cannot lose the choice.
    #[serde(default)]
    pub target_language: Option<String>,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ImageSaveLocation {
    Clipboard,
    #[default]
    Pictures,
    Documents,
    // Custom(PathBuf), // TODO
}

// TODO: Use type from screenshot directly?
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Choice {
    Output(Option<String>),
    Rectangle,
    Window,
}

impl From<&mut Choice> for Choice {
    fn from(value: &mut Choice) -> Self {
        // Convenience implementation to move Choice so that the borrow checker doesn't complain
        // about partial moves
        match value {
            Choice::Output(output) => Choice::Output(output.take()),
            Choice::Rectangle => Choice::Rectangle,
            Choice::Window => Choice::Window,
        }
    }
}

impl Default for Choice {
    fn default() -> Self {
        Choice::Output(None)
    }
}
