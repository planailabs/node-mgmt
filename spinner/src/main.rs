//! Minimal cross-platform splash window, rendered on the CPU (no GPU/GL/D3D).
//!
//! Two modes:
//!   - default (indeterminate): an animated ring spinner + optional `--text` label,
//!     shown while the launcher mounts the runtime; closed when the launcher kills
//!     it. Does NOT read stdin (so an inherited /dev/null can't close it early).
//!   - `--progress` (determinate): a progress bar fed over STDIN, used while a
//!     staged update is applied. Protocol (zenity-compatible), one line each:
//!       `<0..100>`  set percent      `#<text>`  set label      EOF / `100` close
//!
//! Rendering uses winit (window) + softbuffer (a CPU framebuffer blitted via the
//! platform's 2D path: GDI / CoreGraphics / X11-Wayland shm). This deliberately
//! avoids any GPU API: eframe's glow needed desktop OpenGL ≥2.0 and wgpu needed a
//! Direct3D adapter — neither exists on a display-only VM (virtio "DOD" GPU) or
//! many RDP/headless sessions, where the old splash silently failed. A CPU blit
//! works anywhere a window can be shown.
//!
//! A hard max-lifetime timer is a safety net if the launcher dies without reaping us.

use std::io::BufRead;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use font8x8::UnicodeFonts;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const CANVAS: u32 = 0x0f_0e_0c;
const BRAND: (u8, u8, u8) = (0xff, 0x6a, 0x3d);
const TEXT: (u8, u8, u8) = (0xe8, 0xe4, 0xde);
const TRACK: (u8, u8, u8) = (0x3a, 0x36, 0x32);
const MAX_LIFETIME: Duration = Duration::from_secs(180);
const FRAME: Duration = Duration::from_millis(33); // ~30fps; plenty for a splash
const WIN_W: u32 = 320;
const WIN_H: u32 = 180;

/// Selftest closes after this many painted frames (proof the window renders) or this
/// wall-clock cap, whichever comes first — so the probe never hangs.
const SELFTEST_FRAMES: u32 = 6;
const SELFTEST_MAX: Duration = Duration::from_secs(5);

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
    /// the splash actually comes up in a given environment (a window + a framebuffer).
    /// Exits non-zero if the window/surface can't be created or never rendered.
    #[arg(long)]
    selftest: bool,
}

#[derive(Default)]
struct Shared {
    frac: f32,
    label: String,
    closed: bool,
}

struct App {
    progress: bool,
    selftest: bool,
    title: String,
    shared: Arc<Mutex<Shared>>,
    started: Instant,
    frames: u32,
    window: Option<Rc<Window>>,
    // Context must outlive the surface, so keep it alive in the struct.
    _context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    init_error: Option<String>,
    stdin_started: bool,
    proxy: winit::event_loop::EventLoopProxy<()>,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let shared = Arc::new(Mutex::new(Shared { frac: 0.0, label: args.text.clone(), closed: false }));

    let event_loop = match EventLoop::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("plan-ai-spinner: cannot create event loop: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    event_loop.set_control_flow(ControlFlow::wait_duration(FRAME));
    let proxy = event_loop.create_proxy();

    let mut app = App {
        progress: args.progress,
        selftest: args.selftest,
        title: args.title.clone(),
        shared,
        started: Instant::now(),
        frames: 0,
        window: None,
        _context: None,
        surface: None,
        init_error: None,
        stdin_started: false,
        proxy,
    };

    let run = event_loop.run_app(&mut app);

    if args.selftest {
        if let Some(err) = &app.init_error {
            eprintln!("plan-ai-spinner: selftest FAILED — {err}");
            return std::process::ExitCode::from(2);
        }
        if let Err(e) = run {
            eprintln!("plan-ai-spinner: selftest FAILED — event loop error: {e}");
            return std::process::ExitCode::from(2);
        }
        if app.frames >= 1 {
            println!("plan-ai-spinner: selftest ok ({} frames rendered)", app.frames);
            return std::process::ExitCode::SUCCESS;
        }
        eprintln!("plan-ai-spinner: selftest FAILED — window opened but never rendered");
        return std::process::ExitCode::from(1);
    }
    if let Err(e) = run {
        eprintln!("plan-ai-spinner: event loop error: {e}");
        return std::process::ExitCode::from(1);
    }
    std::process::ExitCode::SUCCESS
}

impl App {
    /// Should the splash close now? (stdin/launcher asked, or the lifetime/selftest cap.)
    fn should_close(&self) -> bool {
        if self.init_error.is_some() {
            return true;
        }
        if self.selftest {
            return self.frames >= SELFTEST_FRAMES || self.started.elapsed() >= SELFTEST_MAX;
        }
        self.shared.lock().map(|s| s.closed).unwrap_or(true) || self.started.elapsed() >= MAX_LIFETIME
    }

    fn render(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else { return };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let (frac, label) = self.shared.lock().map(|s| (s.frac, s.label.clone())).unwrap_or((0.0, String::new()));
        let scale = (w as f32 / WIN_W as f32).max(1.0); // honour HiDPI physical size

        for px in buf.iter_mut() {
            *px = CANVAS;
        }
        let mut fb = Frame { buf: &mut buf, w, h };
        if self.progress {
            draw_progress(&mut fb, scale, frac);
        } else {
            draw_spinner(&mut fb, scale, self.started.elapsed().as_secs_f32());
        }
        if !label.is_empty() {
            draw_text_centered(&mut fb, scale, &label, (h as f32 * 0.72) as i32, TEXT);
        }
        let _ = buf.present();
        self.frames += 1;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already created (e.g. resume after suspend)
        }
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(LogicalSize::new(WIN_W, WIN_H))
            .with_resizable(false)
            .with_decorations(false);
        let window = match event_loop.create_window(attrs) {
            Ok(win) => Rc::new(win),
            Err(e) => {
                self.init_error = Some(format!("could not create window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.init_error = Some(format!("could not create softbuffer context: {e}"));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                self.init_error = Some(format!("could not create softbuffer surface: {e}"));
                event_loop.exit();
                return;
            }
        };
        // Only the determinate mode reads stdin (the launcher pipes it). The
        // indeterminate splash is killed by the launcher, so it must not treat an
        // inherited /dev/null EOF as "close".
        if self.progress && !self.selftest && !self.stdin_started {
            spawn_stdin_reader(self.shared.clone(), self.proxy.clone());
            self.stdin_started = true;
        }
        window.request_redraw();
        self._context = Some(context);
        self.surface = Some(surface);
        self.window = Some(window);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A stdin update arrived — repaint promptly instead of waiting for the tick.
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.should_close() {
            event_loop.exit();
            return;
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::wait_duration(FRAME));
    }
}

/// Read the zenity-style progress protocol from stdin on a background thread,
/// updating shared state and nudging the event loop to repaint after each line.
fn spawn_stdin_reader(shared: Arc<Mutex<Shared>>, proxy: winit::event_loop::EventLoopProxy<()>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if let Ok(mut s) = shared.lock() {
                if let Some(text) = line.strip_prefix('#') {
                    s.label = text.trim().to_string();
                } else if let Ok(pct) = line.parse::<u32>() {
                    s.frac = (pct.min(100) as f32) / 100.0;
                    if pct >= 100 {
                        s.closed = true;
                    }
                }
            }
            let _ = proxy.send_event(());
        }
        // EOF: the launcher closed the pipe → close the window.
        if let Ok(mut s) = shared.lock() {
            s.closed = true;
        }
        let _ = proxy.send_event(());
    });
}

// ---------------------------------------------------------------------------
// CPU drawing — a tiny software rasterizer over softbuffer's 0x00RRGGBB buffer.
// ---------------------------------------------------------------------------

struct Frame<'a> {
    buf: &'a mut [u32],
    w: u32,
    h: u32,
}

fn rgb((r, g, b): (u8, u8, u8)) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Linear blend of two colours by `t` in 0..=1 (for the spinner trail fade).
fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

impl Frame<'_> {
    #[inline]
    fn put(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as u32) < self.w && (y as u32) < self.h {
            self.buf[(y as u32 * self.w + x as u32) as usize] = color;
        }
    }
    fn fill_rect(&mut self, x: i32, y: i32, rw: i32, rh: i32, color: u32) {
        for dy in 0..rh {
            for dx in 0..rw {
                self.put(x + dx, y + dy, color);
            }
        }
    }
    fn fill_disc(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        let r2 = r * r;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r2 {
                    self.put(cx + dx, cy + dy, color);
                }
            }
        }
    }
}

/// Indeterminate spinner: a ring of dots with a head that brightens and a fading trail.
fn draw_spinner(fb: &mut Frame, scale: f32, t: f32) {
    const DOTS: usize = 12;
    let cx = fb.w as i32 / 2;
    let cy = (fb.h as f32 * 0.40) as i32;
    let radius = 30.0 * scale;
    let dot_r = (4.0 * scale).round() as i32;
    let head = (t * 1.3) * DOTS as f32; // ~1.3 rev/s
    for i in 0..DOTS {
        let ang = (i as f32 / DOTS as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let x = cx + (ang.cos() * radius) as i32;
        let y = cy + (ang.sin() * radius) as i32;
        // Distance (in dot-steps) behind the rotating head → brightness.
        let mut d = (head - i as f32).rem_euclid(DOTS as f32);
        d /= DOTS as f32; // 0 = head, →1 = far trail
        let bright = 1.0 - d;
        fb.fill_disc(x, y, dot_r, rgb(lerp(TRACK, BRAND, bright * bright)));
    }
}

/// Determinate progress bar: a track with a BRAND-filled portion + a percentage label.
fn draw_progress(fb: &mut Frame, scale: f32, frac: f32) {
    let frac = frac.clamp(0.0, 1.0);
    let bw = (220.0 * scale) as i32;
    let bh = (16.0 * scale) as i32;
    let x = fb.w as i32 / 2 - bw / 2;
    let y = (fb.h as f32 * 0.40) as i32;
    fb.fill_rect(x, y, bw, bh, rgb(TRACK));
    let fill = (bw as f32 * frac) as i32;
    fb.fill_rect(x, y, fill, bh, rgb(BRAND));
    let pct = format!("{}%", (frac * 100.0).round() as i32);
    draw_text_centered(fb, scale, &pct, y + bh + (10.0 * scale) as i32, TEXT);
}

/// Blit a string of 8x8 bitmap glyphs centred horizontally at the given top `y`.
/// Unknown/unsupported chars render as blank cells. `scale` upsizes each pixel.
fn draw_text_centered(fb: &mut Frame, scale: f32, text: &str, y: i32, color: (u8, u8, u8)) {
    let px = (scale.round() as i32).max(1);
    let cell = 8 * px;
    let chars: Vec<char> = text.chars().collect();
    let total_w = chars.len() as i32 * cell;
    let mut x = fb.w as i32 / 2 - total_w / 2;
    let c = rgb(color);
    for ch in chars {
        if let Some(glyph) = glyph_for(ch) {
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if bits & (1 << col) != 0 {
                        fb.fill_rect(x + col * px, y + row as i32 * px, px, px, c);
                    }
                }
            }
        }
        x += cell;
    }
}

/// font8x8 glyph for a char, trying Basic Latin then Latin-1 (umlauts etc.). A few
/// common non-Latin-1 punctuation chars degrade to an ASCII lookalike so localized
/// labels (e.g. an ellipsis) don't leave gaps.
fn glyph_for(ch: char) -> Option<[u8; 8]> {
    let ch = match ch {
        '\u{2026}' => '.', // … → a dot (we draw three by the loop only if present; one is fine)
        '\u{2014}' | '\u{2013}' => '-',
        '\u{00A0}' => ' ',
        other => other,
    };
    font8x8::BASIC_FONTS.get(ch).or_else(|| font8x8::LATIN_FONTS.get(ch))
}
