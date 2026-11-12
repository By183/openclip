//! Update UI: in-app GitHub checks and self-update downloads are disabled.
//! Keeps the static version / update status row on the About page and local
//! reporting if `openclip_hook64.dll` is missing beside the executable.

use eframe::egui::{self, RichText};

use super::theme::*;
use super::widgets::*;
use super::{App, icons};
use crate::t;

/// State of the game-capture sidecar beside the executable.
pub(super) enum RepairState {
    /// The sidecar is there, or this platform has none.
    Present,
    /// Missing beside the executable.
    Missing,
}

impl App {
    /// What the Game card / About card says about the sidecar, if anything.
    pub(super) fn repair_status(&self) -> Option<(String, egui::Color32)> {
        match &self.repair {
            RepairState::Present => None,
            RepairState::Missing => Some((t!(HOOK_MISSING).into(), ORANGE)),
        }
    }

    /// About → Updates: disabled check button + static note, and local sidecar warning if missing.
    pub(super) fn about_update_rows(&mut self, ui: &mut egui::Ui) {
        Card::show(ui, |card| {
            card.row_inline("", |ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(false, |ui| {
                        let _ = tinted_button_small(ui, &format!("{}  {}", icons::REFRESH, t!(UPDATE_CHECK_BUTTON)));
                    });
                    ui.label(RichText::new(t!(UPDATE_DISABLED)).color(LABEL_2));
                });
            });
            if let Some((text, colour)) = self.repair_status() {
                card.custom(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(text).color(colour));
                    });
                });
            }
        });
        footnote(ui, t!(UPDATES_NOTE));
    }
}
