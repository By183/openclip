#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::File;
use std::sync::Mutex;
use eframe::egui;

/// Application icon (same artwork as `assets/favicon.ico`), decoded at startup
/// for the window title bar / taskbar / dock.
const APP_ICON_PNG: &[u8] = openclip::video::watermark::LOGO_PNG;

/// Thread-safe writer that fans out log output to stderr and/or openclip.log.
struct DualWriter {
    file: Option<Mutex<File>>,
}

impl std::io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write(buf);
        if let Some(f) = &self.file
            && let Ok(mut guard) = f.lock()
        {
            let _ = guard.write_all(buf);
            let _ = guard.flush();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        if let Some(f) = &self.file
            && let Ok(mut guard) = f.lock()
        {
            let _ = guard.flush();
        }
        Ok(())
    }
}

fn init_logging() {
    let log_file = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("openclip.log")))
        .and_then(|path| File::create(path).ok())
        .map(Mutex::new);

    let writer = DualWriter { file: log_file };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(writer)))
        .init();
}

fn main() -> eframe::Result {
    init_logging();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("openclip")
            .with_app_id("openclip")
            .with_inner_size(openclip::ui::WINDOW_SIZE)
            .with_min_inner_size(openclip::ui::WINDOW_SIZE)
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native("openclip", options, Box::new(|cc| Ok(Box::new(openclip::ui::App::new(cc)))))
}

fn app_icon() -> egui::IconData {
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => icon,
        Err(e) => {
            log::warn!("failed to decode app icon: {e}");
            egui::IconData::default()
        }
    }
}
