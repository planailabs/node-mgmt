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
    /// Self-test: open the window, render a few frames, then exit 0. Used to verify
    /// the splash actually comes up in a given environment (display + GL present).
    /// Exits non-zero if the window can't be created or never rendered.
    #[arg(long)]
    selftest: bool,
}

#[derive(Default)]
struct Shared {
    frac: f32,
    label: String,
    closed: bool,
}

/// eframe wgpu config that PRE-CREATES the adapter + device itself (WgpuSetup::Existing)
/// instead of letting eframe call `request_adapter` with `force_fallback_adapter =
/// false`. That default skips software adapters AND relies on adapter *enumeration*,
/// which returns nothing in a headless / session-0 context (RDP, a service, an ssh
/// launch) — so the splash never came up on a GPU-less Windows box ("no suitable
/// adapter found"). Here we try a real GPU first, then fall back to forcing a software
/// adapter: `force_fallback_adapter = true` CREATES the DX12 WARP rasterizer directly
/// (no enumeration), so the splash renders with no GPU and no working system OpenGL.
/// If even that fails we hand back eframe's default config (real-GPU machines still
/// work; the launcher's --selftest gate covers the rest).
#[cfg(target_os = "windows")]
fn wgpu_adapter_config() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{wgpu, WgpuConfiguration, WgpuSetup, WgpuSetupExisting};
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY | wgpu::Backends::GL),
        ..Default::default()
    });
    let request = |fallback| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: fallback,
        }))
    };
    if let Some(adapter) = request(false).or_else(|| request(true)) {
        let dev = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("plan-ai-spinner"),
                required_features: wgpu::Features::empty(),
                // The adapter's own limits — guaranteed satisfiable (WARP advertises
                // generous ones), and enough for a 320×180 splash.
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ));
        if let Ok((device, queue)) = dev {
            return WgpuConfiguration {
                wgpu_setup: WgpuSetup::Existing(WgpuSetupExisting { instance, adapter, device, queue }),
                ..Default::default()
            };
        }
    }
    WgpuConfiguration::default()
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let shared = Arc::new(Mutex::new(Shared { frac: 0.0, label: args.text.clone(), closed: false }));
    // Counts frames the app actually painted — the selftest's proof the window came up.
    let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));

    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 180.0])
            .with_resizable(false)
            .with_decorations(false),
        ..Default::default()
    };
    // Windows uses the wgpu backend with a pre-created (WARP-capable) adapter; mac/linux
    // use glow, whose default options are correct.
    #[cfg(target_os = "windows")]
    {
        options.wgpu_options = wgpu_adapter_config();
    }
    let progress = args.progress;
    let selftest = args.selftest;
    let title = args.title.clone();
    let shared_for_app = shared.clone();
    let frames_for_app = frames.clone();
    let res = eframe::run_native(
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
            if progress && !selftest {
                spawn_stdin_reader(shared.clone(), cc.egui_ctx.clone());
            }
            // Selftest needs continuous repaints so it reaches the close-after-N-frames
            // condition without waiting on input events.
            if selftest {
                cc.egui_ctx.request_repaint();
            }
            Ok(Box::new(SpinnerApp {
                started: Instant::now(),
                progress,
                selftest,
                frames: frames_for_app,
                shared: shared_for_app,
            }))
        }),
    );

    if selftest {
        match res {
            Ok(()) => {
                let n = frames.load(std::sync::atomic::Ordering::Relaxed);
                if n >= 1 {
                    println!("plan-ai-spinner: selftest ok ({n} frames rendered)");
                    std::process::exit(0);
                }
                eprintln!("plan-ai-spinner: selftest FAILED — window opened but never rendered");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("plan-ai-spinner: selftest FAILED — could not create window: {e}");
                std::process::exit(2);
            }
        }
    }
    res
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
    selftest: bool,
    frames: Arc<std::sync::atomic::AtomicU32>,
    shared: Arc<Mutex<Shared>>,
}

/// Selftest closes after this many painted frames (proof the window renders) or
/// this wall-clock cap, whichever comes first — so the test never hangs.
const SELFTEST_FRAMES: u32 = 6;
const SELFTEST_MAX: Duration = Duration::from_secs(5);

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
        let painted = self.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if self.selftest {
            // Keep animating until we've proven a few frames, then close cleanly.
            ctx.request_repaint();
            if painted >= SELFTEST_FRAMES || self.started.elapsed() >= SELFTEST_MAX {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
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
