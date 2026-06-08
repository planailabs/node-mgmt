//! Minimal cross-platform splash: a borderless window showing nothing but an
//! animated spinner. The launcher spawns it while it mounts/extracts the runtime
//! components (the slow first-run phase that happens BEFORE Electron's window
//! appears) and kills it once the app is ready — Electron POSTs /api/ready to the
//! launcher's localhost server, which kills this process.
//!
//! A hard max-lifetime timer is a safety net: if the launcher crashes or is killed
//! and never reaps us, the splash closes itself rather than lingering forever.

use eframe::egui;
use std::time::{Duration, Instant};

// Brand colours (match the SPA's --c-canvas dark background + the .ai accent).
const CANVAS: egui::Color32 = egui::Color32::from_rgb(0x0f, 0x0e, 0x0c);
const BRAND: egui::Color32 = egui::Color32::from_rgb(0xff, 0x6a, 0x3d);
const MAX_LIFETIME: Duration = Duration::from_secs(180);

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([200.0, 200.0])
            .with_resizable(false)
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "plan.ai",
        options,
        Box::new(|cc| {
            // Dark theme with the brand canvas as the panel fill so the borderless
            // window is one flat dark square behind the spinner.
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals = egui::Visuals::dark();
            style.visuals.panel_fill = CANVAS;
            cc.egui_ctx.set_style(style);
            Ok(Box::new(SpinnerApp { started: Instant::now() }))
        }),
    )
}

struct SpinnerApp {
    started: Instant,
}

impl eframe::App for SpinnerApp {
    // Paint the window (incl. before the first egui frame) the brand canvas colour.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = CANVAS.to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.started.elapsed() >= MAX_LIFETIME {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // The Spinner widget requests its own repaints, so update() runs each frame
        // (keeping the lifetime check live) without us burning CPU when idle.
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.add(egui::Spinner::new().size(48.0).color(BRAND));
            });
        });
    }
}
