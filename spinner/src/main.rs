//! Minimal cross-platform splash window. Two modes:
//!   - default (indeterminate): an animated spinner + optional `--text` label,
//!     shown while the launcher mounts the runtime; closed when the launcher kills
//!     it. Does NOT read stdin (so an inherited /dev/null can't close it early).
//!   - `--progress` (determinate): a progress bar fed over STDIN, used while a
//!     staged update is applied. Protocol (zenity-compatible), one line each:
//!       `<0..100>`  set percent      `#<text>`  set label      EOF / `100` close
//!
//! A hard max-lifetime timer is a safety net if the launcher dies without reaping us.

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;

const CANVAS: egui::Color32 = egui::Color32::from_rgb(0x0f, 0x0e, 0x0c);
const BRAND: egui::Color32 = egui::Color32::from_rgb(0xff, 0x6a, 0x3d);
const MAX_LIFETIME: Duration = Duration::from_secs(180);

#[derive(Parser)]
#[command(name = "plan-ai-spinner", about = "plan.ai splash spinner")]
struct Args {
    /// Label shown under the spinner / bar.
    #[arg(long, default_value = "")]
    text: String,
    /// Window title.
    #[arg(long, default_value = "plan.ai")]
    title: String,
    /// Determinate progress-bar mode: read 0..100 percentages + `#labels` on stdin.
    #[arg(long)]
    progress: bool,
}

#[derive(Default)]
struct Shared {
    frac: f32,
    label: String,
    closed: bool,
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let shared = Arc::new(Mutex::new(Shared { frac: 0.0, label: args.text.clone(), closed: false }));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 180.0])
            .with_resizable(false)
            .with_decorations(false),
        ..Default::default()
    };
    let progress = args.progress;
    let title = args.title.clone();
    let shared_for_app = shared.clone();
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals = egui::Visuals::dark();
            style.visuals.panel_fill = CANVAS;
            cc.egui_ctx.set_style(style);
            // Only the determinate mode reads stdin (the launcher pipes it). The
            // indeterminate splash is killed by the launcher, so it must not treat
            // an inherited /dev/null EOF as "close".
            if progress {
                spawn_stdin_reader(shared.clone(), cc.egui_ctx.clone());
            }
            Ok(Box::new(SpinnerApp { started: Instant::now(), progress, shared: shared_for_app }))
        }),
    )
}

/// Read the zenity-style progress protocol from stdin on a background thread,
/// updating shared state and requesting a repaint after each line.
fn spawn_stdin_reader(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            let mut s = shared.lock().unwrap();
            if let Some(text) = line.strip_prefix('#') {
                s.label = text.trim().to_string();
            } else if let Ok(pct) = line.parse::<u32>() {
                s.frac = (pct.min(100) as f32) / 100.0;
                if pct >= 100 {
                    s.closed = true;
                }
            }
            drop(s);
            ctx.request_repaint();
        }
        // EOF: the launcher closed the pipe → close the window.
        shared.lock().unwrap().closed = true;
        ctx.request_repaint();
    });
}

struct SpinnerApp {
    started: Instant,
    progress: bool,
    shared: Arc<Mutex<Shared>>,
}

impl eframe::App for SpinnerApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = CANVAS.to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (frac, label, closed) = {
            let s = self.shared.lock().unwrap();
            (s.frac, s.label.clone(), s.closed)
        };
        if closed || self.started.elapsed() >= MAX_LIFETIME {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                if self.progress {
                    ui.add_sized(
                        [220.0, 18.0],
                        egui::ProgressBar::new(frac).fill(BRAND).show_percentage(),
                    );
                } else {
                    ui.add(egui::Spinner::new().size(48.0).color(BRAND));
                }
                if !label.is_empty() {
                    ui.add_space(14.0);
                    ui.label(label);
                }
            });
        });
    }
}
