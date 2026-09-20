#![allow(non_snake_case)]
//
// Linux backend, built on x11rb — pure-Rust XCB protocol bindings with no
// libX11/libxcb C dependency and no bindgen step. This mirrors the
// philosophy of ui/windows.rs: no widget toolkit, no retained-mode scene
// graph, just direct core-protocol requests and manual pixel writes into a
// backing Pixmap. It talks to the X server directly, so it runs unmodified
// under both native X11 and XWayland (i.e. on every mainstream Wayland
// compositor today); a from-scratch wayland-client/wlr-layer-shell backend
// would only matter for the handful of compositors that ship no XWayland,
// and would roughly double this file for a niche most people won't hit.
//
// There is no equivalent of Win32 buttons/statusbar here — those are drawn
// and hit-tested by hand, same spirit as the raw pixel maze rendering on
// the Windows side.

use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering::*;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

use crate::benchmark::{self, BFS_TARGET, ASTAR_TARGET, MazeSnapshot, SharedState};
use crate::maze::Cell;
use crate::scoring::{self, ScoreReport};
use crate::solver::Algorithm;

// ─── Layout constants (kept identical to the Windows build) ──────────────────

const WIN_W: u16 = 900;
const WIN_H: u16 = 638;

const CELL_PX: i32 = 5;
const X_OFF:   i32 = 5;
const Y_OFF:   i32 = 5;
const BUF_W:   i32 = 510;
const BUF_H:   i32 = 510;

const BTN_RUN:     Rect = Rect { x: 10,  y: 578, w: 160, h: 30 };
const BTN_WEBSITE: Rect = Rect { x: 180, y: 578, w: 160, h: 30 };
const BTN_COPY:    Rect = Rect { x: 350, y: 578, w: 160, h: 30 };

#[derive(Clone, Copy)]
struct Rect { x: i32, y: i32, w: i32, h: i32 }
impl Rect {
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

// ─── Colours (0x00RRGGBB, matching the COLORREF-derived constants above) ─────

const COL_WALL:    u32 = 0x1A1A1A;
const COL_FLOOR:   u32 = 0xF0F0F0;
const COL_EXP_BFS: u32 = 0xF0D8C8;
const COL_EXP_AST: u32 = 0xF0E8C8;
const COL_PATH:    u32 = 0xC85000;
const COL_START:   u32 = 0x00A000;
const COL_END:     u32 = 0x0000C0;
const COL_HEADER:  u32 = 0xF0F0F0;
const COL_BORDER:  u32 = 0xCCCCCC;
const COL_TITLE:   u32 = 0x800000;
const COL_LINK:    u32 = 0x604000;
const COL_TEXT:    u32 = 0x000000;
const COL_LABEL:   u32 = 0x505050;
const COL_SECTION: u32 = 0x404040;
const COL_WINDOW:  u32 = 0xFFFFFF;
const COL_SCORE:   u32 = 0x003080;
const COL_BTNFACE: u32 = 0xE8E8E8;
const COL_BTNEDGE: u32 = 0x888888;

// Note: BFS/A* explored colours are swapped R<->B relative to the
// COLORREF(0x00BBGGRR)-packed Windows values, since here we store true
// 0xRRGGBB straight into the image buffer with no byte-order inversion.

fn tier_color(tier: &str) -> u32 {
    match tier {
        "Very Low End" | "Low End"  => 0xB00000,
        "Below Average" | "Average" => 0x000000,
        "Good" | "Great"            => 0x008000,
        "Excellent" | "Extreme"     => 0xC00000,
        _                            => 0x000000,
    }
}
fn mult_sym(m: f64) -> &'static str {
    if m >= 1.05 { "+" } else if m >= 0.95 { "~" } else { "-" }
}
fn mult_color(m: f64) -> u32 {
    if m >= 1.05 { 0x008000 } else if m >= 0.95 { 0xA0A000 } else { 0xC00000 }
}

// ─── App state ────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum AppState { Ready, SeedChecking, Running, Done }

struct App {
    conn: RustConnection,
    window: Window,
    gc: Gcontext,
    font: Font,
    depth: u8,
    byte_order_msb: bool,

    maze_pix:    Pixmap,
    stats_pix:   Pixmap,
    results_pix: Pixmap,

    shared: Arc<SharedState>,
    num_threads: usize,

    state: AppState,
    run_label: String,
    copy_visible: bool,
    title_suffix: String,

    score_report: Option<ScoreReport>,
    last_snapshot: Option<MazeSnapshot>,
    last_rendered_seed: u64,
    last_rendered_algo: Option<Algorithm>,
    bench_start: Option<Instant>,

    pending_seeds: Arc<Mutex<Option<Vec<u64>>>>,

    // ICCCM selection (clipboard) state
    clipboard_text: Option<String>,
    atom_clipboard: Atom,
    atom_targets: Atom,
    atom_utf8: Atom,
    atom_tick: Atom,
    atom_seeds_ready: Atom,
    atom_wm_protocols: Atom,
    atom_wm_delete: Atom,
}

pub fn run(shared: Arc<SharedState>, num_threads: usize) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let depth = screen.root_depth;
    let byte_order_msb = conn.setup().image_byte_order == ImageOrder::MSB_FIRST;

    let window = conn.generate_id()?;
    let values = CreateWindowAux::new()
        .background_pixel(COL_BTNFACE)
        .event_mask(
            EventMask::EXPOSURE
                | EventMask::BUTTON_PRESS
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::KEY_PRESS,
        );
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0, 0, WIN_W, WIN_H,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &values,
    )?;

    // Fixed-size window (disable resize), matching the Windows build.
    let hints = WmSizeHints {
        flags: WmSizeHintsFlags::P_MIN_SIZE | WmSizeHintsFlags::P_MAX_SIZE,
        min_width: Some(WIN_W as i32), min_height: Some(WIN_H as i32),
        max_width: Some(WIN_W as i32), max_height: Some(WIN_H as i32),
        ..Default::default()
    };
    hints.set_normal_hints(&conn, window)?;

    let title = b"MazeBench v1.2.2";
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title)?;
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_ICON_NAME, AtomEnum::STRING, title)?;

    let atom_wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let atom_wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    conn.change_property32(PropMode::REPLACE, window, atom_wm_protocols, AtomEnum::ATOM, &[atom_wm_delete])?;

    let atom_clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let atom_targets = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;
    let atom_utf8 = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let atom_tick = conn.intern_atom(false, b"MAZEBENCH_TICK")?.reply()?.atom;
    let atom_seeds_ready = conn.intern_atom(false, b"MAZEBENCH_SEEDS_READY")?.reply()?.atom;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?;

    // Core font. "fixed" ships with every X server (it's the guaranteed
    // built-in), so we don't risk a missing-font panic on minimal setups.
    let font = conn.generate_id()?;
    conn.open_font(font, b"fixed")?;

    let maze_pix = conn.generate_id()?;
    conn.create_pixmap(depth, maze_pix, window, BUF_W as u16, BUF_H as u16)?;
    let stats_pix = conn.generate_id()?;
    conn.create_pixmap(depth, stats_pix, window, 390, BUF_H as u16)?;
    let results_pix = conn.generate_id()?;
    conn.create_pixmap(depth, results_pix, window, WIN_W, 510)?;

    conn.map_window(window)?;
    conn.flush()?;

    let mut app = App {
        conn, window, gc, font, depth, byte_order_msb,
        maze_pix, stats_pix, results_pix,
        shared, num_threads,
        state: AppState::Ready,
        run_label: "Run Benchmark".to_string(),
        copy_visible: false,
        title_suffix: String::new(),
        score_report: None,
        last_snapshot: None,
        last_rendered_seed: 0,
        last_rendered_algo: None,
        bench_start: None,
        pending_seeds: Arc::new(Mutex::new(None)),
        clipboard_text: None,
        atom_clipboard, atom_targets, atom_utf8,
        atom_tick, atom_seeds_ready,
        atom_wm_protocols, atom_wm_delete,
    };

    app.fill_backbuffer(app.maze_pix, BUF_W, BUF_H, COL_WALL)?;

    // 16ms ticker thread, standing in for the Win32 SetTimer(..., 16, ...).
    // It just wakes the event loop by posting a ClientMessage to our own
    // window; all real work still happens on the main (UI) thread.
    {
        let conn2 = x11rb::connect(None)?.0;
        let win = app.window;
        let atom = app.atom_tick;
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(16));
            let ev = ClientMessageEvent::new(32, win, atom, [0u32; 5]);
            if conn2.send_event(false, win, EventMask::NO_EVENT, ev).is_err() { break; }
            if conn2.flush().is_err() { break; }
        });
    }

    app.event_loop()?;
    Ok(())
}

impl App {
    fn event_loop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let event = self.conn.wait_for_event()?;
            let mut events = vec![event];
            while let Some(ev) = self.conn.poll_for_event()? { events.push(ev); }

            for event in events {
                match event {
                    Event::Expose(e) if e.count == 0 => self.paint()?,
                    Event::ButtonPress(e) => self.on_button_press(e.event_x as i32, e.event_y as i32)?,
                    Event::SelectionRequest(e) => self.on_selection_request(e)?,
                    Event::SelectionClear(_) => { self.clipboard_text = None; }
                    Event::ClientMessage(e) => {
                        if e.type_ == self.atom_tick {
                            self.on_tick()?;
                        } else if e.type_ == self.atom_seeds_ready {
                            self.on_seeds_ready()?;
                        } else if e.type_ == self.atom_wm_protocols {
                            let data = e.data.as_data32();
                            if data[0] == self.atom_wm_delete {
                                self.shared.done.store(true, Release);
                                return Ok(());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // ─── Tick (≈ WM_TIMER) ────────────────────────────────────────────────

    fn on_tick(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.state == AppState::Ready || self.state == AppState::SeedChecking {
            return Ok(());
        }

        if let Ok(mut snap_lock) = self.shared.latest_snapshot.try_lock() {
            if let Some(snap) = snap_lock.take() {
                self.last_snapshot = Some(snap);
            }
        }

        let mut dirty = false;
        if let Some(ref snap) = self.last_snapshot {
            let seed_changed = snap.seed != self.last_rendered_seed;
            let algo_changed = self.last_rendered_algo.map(|a| a != snap.algorithm).unwrap_or(true);
            if seed_changed || algo_changed {
                self.last_rendered_seed = snap.seed;
                self.last_rendered_algo = Some(snap.algorithm);
                self.render_maze_to_backbuffer()?;
                dirty = true;
            }
        }

        if self.shared.done.load(Acquire) && self.state == AppState::Running {
            let report = scoring::compute(&self.shared, self.num_threads);
            self.score_report = Some(report);
            self.state = AppState::Done;
            self.run_label = "Run Benchmark".to_string();
            self.copy_visible = true;
            self.title_suffix = " — Done".to_string();
            self.set_title()?;
            dirty = true;
        }

        if dirty || self.state == AppState::Running {
            // Repaint everything; stats/progress show live numbers every tick.
            let (w, h) = (WIN_W as i32, WIN_H as i32);
            self.conn.clear_area(false, self.window, 0, 0, w as u16, h as u16)?;
            self.paint()?;
        }
        Ok(())
    }

    // ─── Buttons ──────────────────────────────────────────────────────────

    fn on_button_press(&mut self, x: i32, y: i32) -> Result<(), Box<dyn std::error::Error>> {
        if BTN_RUN.contains(x, y) && self.state != AppState::SeedChecking {
            match self.state {
                AppState::Ready => {
                    self.run_label = "Checking seeds...".to_string();
                    self.state = AppState::SeedChecking;
                    self.paint()?;

                    let slot = Arc::clone(&self.pending_seeds);
                    let conn2 = x11rb::connect(None)?.0;
                    let win = self.window;
                    let atom = self.atom_seeds_ready;
                    std::thread::spawn(move || {
                        let seeds = crate::maze::load_seeds();
                        *slot.lock().unwrap() = Some(seeds);
                        let ev = ClientMessageEvent::new(32, win, atom, [0u32; 5]);
                        let _ = conn2.send_event(false, win, EventMask::NO_EVENT, ev);
                        let _ = conn2.flush();
                    });
                }
                AppState::Running => {
                    self.shared.done.store(true, Release);
                    let report = scoring::compute(&self.shared, self.num_threads);
                    self.score_report = Some(report);
                    self.state = AppState::Done;
                    self.run_label = "Run Benchmark".to_string();
                    self.copy_visible = true;
                    self.title_suffix = " — Stopped".to_string();
                    self.set_title()?;
                    self.paint()?;
                }
                _ => {}
            }
        } else if BTN_WEBSITE.contains(x, y) {
            let _ = std::process::Command::new("xdg-open")
                .arg("https://mazebench.mikeden.site/")
                .spawn();
        } else if BTN_COPY.contains(x, y) && self.copy_visible {
            if let Some(ref report) = self.score_report {
                self.clipboard_text = Some(clipboard_text(report));
                self.conn.set_selection_owner(self.window, AtomEnum::PRIMARY, x11rb::CURRENT_TIME)?;
                self.conn.set_selection_owner(self.window, self.atom_clipboard, x11rb::CURRENT_TIME)?;
                self.conn.flush()?;
            }
        }
        Ok(())
    }

    fn on_seeds_ready(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let seeds = {
            let mut slot = self.pending_seeds.lock().unwrap();
            slot.take().unwrap_or_else(|| vec![crate::maze::FALLBACK_SEED])
        };
        self.run_label = "Stop Benchmark".to_string();
        self.state = AppState::Running;
        self.bench_start = Some(Instant::now());
        benchmark::start(Arc::clone(&self.shared), self.num_threads, Arc::new(seeds));
        self.paint()?;
        Ok(())
    }

    fn set_title(&self) -> Result<(), Box<dyn std::error::Error>> {
        let t = format!("MazeBench v1.2.2{}", self.title_suffix);
        self.conn.change_property8(PropMode::REPLACE, self.window, AtomEnum::WM_NAME, AtomEnum::STRING, t.as_bytes())?;
        Ok(())
    }

    // ─── Clipboard (ICCCM selection owner) ───────────────────────────────

    fn on_selection_request(&mut self, e: SelectionRequestEvent) -> Result<(), Box<dyn std::error::Error>> {
        let mut notify_property = e.property;
        let mut success = false;

        if let Some(ref text) = self.clipboard_text {
            if e.target == self.atom_targets {
                let targets = [self.atom_utf8, AtomEnum::STRING.into()];
                self.conn.change_property32(PropMode::REPLACE, e.requestor, e.property, AtomEnum::ATOM, &targets)?;
                success = true;
            } else if e.target == self.atom_utf8 || e.target == AtomEnum::STRING.into() {
                self.conn.change_property8(PropMode::REPLACE, e.requestor, e.property, e.target, text.as_bytes())?;
                success = true;
            }
        }
        if !success {
            notify_property = x11rb::NONE;
        }

        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: notify_property,
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    // ─── Painting ─────────────────────────────────────────────────────────

    fn paint(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.window;

        // Header bar.
        self.fill_rect(w, 0, 0, WIN_W as i32, 36, COL_HEADER)?;
        self.draw_text(w, 10, 24, "MazeBench v1.2.2", COL_TITLE)?;
        self.draw_text_right(w, 890, 24, "mazebench.mikeden.site", COL_LINK)?;
        self.hline(w, 0, 900, 35, COL_BORDER)?;

        match self.state {
            AppState::Done => {
                if let Some(report) = self.score_report.clone() {
                    self.draw_results_screen(&report)?;
                    self.conn.copy_area(self.results_pix, w, self.gc, 0, 36, 0, 36, WIN_W, 510)?;
                }
            }
            _ => {
                self.conn.copy_area(self.maze_pix, w, self.gc, 0, 0, 0, 36, BUF_W as u16, BUF_H as u16)?;
                self.rect_outline(w, 0, 36, BUF_W, 510, COL_BORDER)?;
                self.draw_stats_panel()?;
                self.conn.copy_area(self.stats_pix, w, self.gc, 0, 0, BUF_W as i16, 36, 390, BUF_H as u16)?;
            }
        }

        self.draw_progress_bar()?;
        self.draw_buttons()?;
        self.conn.flush()?;
        Ok(())
    }

    fn draw_progress_bar(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.window;
        let bfs = self.shared.bfs_completed.load(Relaxed);
        let astar = self.shared.astar_completed.load(Relaxed);
        let target = (BFS_TARGET + ASTAR_TARGET) as u64;
        let total = (bfs + astar).min(target);
        let pct = total as f64 / target as f64;

        self.fill_rect(w, 0, 546, 900, 28, COL_WINDOW)?;
        let fill_w = (900.0 * pct) as i32;
        self.fill_rect(w, 0, 546, fill_w, 28, COL_PATH)?;
        self.rect_outline(w, 0, 546, 900, 28, COL_BORDER)?;

        let pct_str = format!("{:.2}%", pct * 100.0);
        // Simplification vs. the Windows build's clip-region trick: draw the
        // label once, centered, in black — legible on both the filled and
        // unfilled halves without needing a clip region round-trip per frame.
        self.draw_text_centered(w, 450, 546 + 18, &pct_str, COL_TEXT)?;
        Ok(())
    }

    fn draw_buttons(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.window;
        self.draw_button(w, BTN_RUN, &self.run_label.clone())?;
        self.draw_button(w, BTN_WEBSITE, "Visit Website")?;
        if self.copy_visible {
            self.draw_button(w, BTN_COPY, "Copy Results")?;
        }
        Ok(())
    }

    fn draw_button(&mut self, d: Drawable, r: Rect, label: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.fill_rect(d, r.x, r.y, r.w, r.h, COL_BTNFACE)?;
        self.rect_outline(d, r.x, r.y, r.w, r.h, COL_BTNEDGE)?;
        self.draw_text_centered(d, r.x + r.w / 2, r.y + r.h / 2 + 5, label, COL_TEXT)?;
        Ok(())
    }

    // ─── Stats panel (local coords: panel spans screen x 510-900, y 36-546) ─

    fn draw_stats_panel(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let bfs = self.shared.bfs_completed.load(Relaxed);
        let astar = self.shared.astar_completed.load(Relaxed);

        let elapsed = self.bench_start.as_ref().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0).max(0.001);
        let secs = elapsed as u64;
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);

        let bfs_per_sec = bfs as f64 / elapsed;
        let astar_per_sec = astar as f64 / elapsed;
        let combined_per_sec = (bfs + astar) as f64 / elapsed;

        let progress = (bfs + astar) as f64 / (BFS_TARGET + ASTAR_TARGET) as f64;
        let eta_str = if progress > 0.01 {
            let total_est = elapsed / progress;
            let remaining = (total_est - elapsed).max(0.0) as u64;
            format!("{:02}:{:02}:{:02}", remaining / 3600, (remaining % 3600) / 60, remaining % 60)
        } else { "??:??:??".to_string() };

        let avg_bfs_ms = self.shared.bfs_times_ns.try_lock().map(|v| mean_u64(&v) / 1_000_000.0).unwrap_or(0.0);
        let avg_astar_ms = self.shared.astar_times_ns.try_lock().map(|v| mean_u64(&v) / 1_000_000.0).unwrap_or(0.0);
        let avg_bfs_exp = self.shared.bfs_explored.try_lock().map(|v| mean_u32(&v)).unwrap_or(0.0);
        let avg_astar_exp = self.shared.astar_explored.try_lock().map(|v| mean_u32(&v)).unwrap_or(0.0);
        let efficiency = if avg_bfs_exp > 0.0 { (1.0 - (avg_astar_exp / avg_bfs_exp).min(1.0)) * 100.0 } else { 0.0 };
        let wrong = self.shared.wrong_solutions.load(Relaxed);
        let show_live = elapsed >= 3.0 && (bfs + astar) >= 200;

        let p = self.stats_pix;
        self.fill_rect(p, 0, 0, 390, 510, COL_WINDOW)?;

        let x = 8i32;
        let mut y = 8i32;
        let line_h = 19i32;

        self.section(p, "Benchmark Progress", x, &mut y)?;
        self.stat_line(p, "BFS Solves", &format!("{:>9} / 400,000", bfs), x, y)?; y += line_h;
        self.stat_line(p, "A* Solves", &format!("{:>12} / 1,200,000", astar), x, y)?; y += line_h;
        y += 4;

        self.section(p, "Throughput", x, &mut y)?;
        self.stat_line(p, "BFS", &format!("{:>8.0} solves/sec", bfs_per_sec), x, y)?; y += line_h;
        self.stat_line(p, "A*", &format!("{:>8.0} solves/sec", astar_per_sec), x, y)?; y += line_h;
        self.stat_line(p, "Combined", &format!("{:>8.0} solves/sec", combined_per_sec), x, y)?; y += line_h;
        y += 4;

        self.section(p, "Timing", x, &mut y)?;
        self.stat_line(p, "Elapsed", &format!("{:02}:{:02}:{:02}", h, m, s), x, y)?; y += line_h;
        self.stat_line(p, "ETA", &eta_str, x, y)?; y += line_h;
        self.stat_line(p, "Avg BFS", &format!("{:.3} ms/solve", avg_bfs_ms), x, y)?; y += line_h;
        self.stat_line(p, "Avg A*", &format!("{:.3} ms/solve", avg_astar_ms), x, y)?; y += line_h;
        y += 4;

        self.section(p, "Exploration", x, &mut y)?;
        self.stat_line(p, "BFS avg explored", &format!("{:>6.0} cells", avg_bfs_exp), x, y)?; y += line_h;
        self.stat_line(p, "A* avg explored", &format!("{:>6.0} cells", avg_astar_exp), x, y)?; y += line_h;
        self.stat_line(p, "A* efficiency", &format!("{:>5.1}%", efficiency), x, y)?; y += line_h;
        y += 4;

        self.section(p, "System", x, &mut y)?;
        self.stat_line(p, "CPU threads", &format!("{}", self.num_threads), x, y)?; y += line_h;
        self.stat_line(p, "Wrong solves", &format!("{}", wrong), x, y)?; y += line_h;
        y += 4;

        self.section(p, "Live Score Estimate", x, &mut y)?;
        if !show_live {
            self.draw_text(p, x, y + 20, "Warming up...", 0x808080)?;
        } else {
            let live_score = (combined_per_sec * 1_000.0).round() as u64;
            self.draw_text_big(p, x, y + 28, &format_score(live_score), COL_SCORE)?;
            y += 32;
            let tier_label = crate::scoring::tier_for(live_score);
            self.draw_text(p, x, y + 18, tier_label, tier_color(tier_label))?;
        }
        Ok(())
    }

    fn section(&mut self, d: Drawable, label: &str, x: i32, y: &mut i32) -> Result<(), Box<dyn std::error::Error>> {
        self.draw_text(d, x, *y + 14, label, COL_SECTION)?;
        *y += 16;
        self.hline(d, x, 385, *y, COL_BORDER)?;
        *y += 2;
        Ok(())
    }

    fn stat_line(&mut self, d: Drawable, label: &str, val: &str, x: i32, y: i32) -> Result<(), Box<dyn std::error::Error>> {
        self.draw_text(d, x, y + 14, label, COL_LABEL)?;
        self.draw_text_right(d, 385, y + 14, val, COL_TEXT)?;
        Ok(())
    }

    // ─── Results screen ───────────────────────────────────────────────────

    fn draw_results_screen(&mut self, report: &ScoreReport) -> Result<(), Box<dyn std::error::Error>> {
        let p = self.results_pix;
        self.fill_rect(p, 0, 36, 900, 510, COL_WINDOW)?;

        self.rect_outline(p, 20, 46, 860, 110, COL_BORDER)?;
        self.draw_text_big_centered(p, 450, 100, &format_score(report.final_score), COL_SCORE)?;
        self.draw_text_centered(p, 450, 138, report.tier, tier_color(report.tier))?;

        self.rect_outline(p, 20, 166, 420, 374, COL_BORDER)?;
        let (lx, rx) = (30i32, 435i32);
        let mut ly = 174i32;
        let lh = 20i32;

        self.res_header(p, "Performance", lx, rx, &mut ly)?;
        self.res_stat(p, "BFS", &format!("{:.0} solves/sec", report.bfs_solves_per_sec), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "A*", &format!("{:.0} solves/sec", report.astar_solves_per_sec), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "Combined", &format!("{:.0} solves/sec", report.combined_solves_per_sec), lx, rx, ly)?; ly += lh;
        let secs = report.total_elapsed_secs as u64;
        self.res_stat(p, "Elapsed", &format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "Threads", &format!("{}", report.num_threads), lx, rx, ly)?; ly += lh;
        ly += 6;

        self.res_header(p, "Quality", lx, rx, &mut ly)?;
        self.res_stat(p, "Avg BFS", &format!("{:.3} ms/solve", report.avg_bfs_time_us / 1000.0), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "Avg A*", &format!("{:.3} ms/solve", report.avg_astar_time_us / 1000.0), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "BFS cells", &format!("{:.0} avg explored", report.avg_bfs_explored), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "A* cells", &format!("{:.0} avg explored", report.avg_astar_explored), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "Gen time", &format!("{:.3} ms/maze", report.avg_gen_time_us / 1000.0), lx, rx, ly)?; ly += lh;
        self.res_stat(p, "Wrong", &format!("{}", report.wrong_count), lx, rx, ly)?;

        self.rect_outline(p, 460, 166, 420, 374, COL_BORDER)?;
        let (bx, bxr) = (470i32, 875i32);
        let mut by = 174i32;
        let bh = 20i32;

        self.res_header(p, "Score Breakdown", bx, bxr, &mut by)?;
        self.res_stat(p, "Speed Base", &format_score(report.speed_base as u64), bx, bxr, by)?; by += bh;

        let mults: [(&str, f64); 8] = [
            ("Path Optimality", report.path_optimality_multiplier),
            ("Expl. Efficiency", report.exploration_efficiency_multiplier),
            ("Consistency", report.consistency_multiplier),
            ("Thermal Sustain", report.thermal_sustain_multiplier),
            ("Parallelism Eff", report.parallelism_efficiency_multiplier),
            ("Peak Burst", report.peak_burst_multiplier),
            ("Algorithm Duel", report.algorithm_duel_multiplier),
            ("Correctness", report.correctness_multiplier),
        ];
        for (label, mult) in mults {
            self.draw_text(p, bx, by + 14, label, COL_LABEL)?;
            self.draw_text_right(p, bxr - 25, by + 14, &format!("x {:.3}", mult), COL_TEXT)?;
            self.draw_text(p, bxr - 18, by + 14, mult_sym(mult), mult_color(mult))?;
            by += bh;
        }

        by += 4;
        self.hline(p, bx, bxr - 5, by, COL_BTNEDGE)?;
        by += 6;
        self.draw_text(p, bx, by + 14, "FINAL SCORE", COL_SCORE)?;
        self.draw_text_right(p, bxr - 5, by + 14, &format_score(report.final_score), COL_TEXT)?;
        Ok(())
    }

    fn res_header(&mut self, d: Drawable, label: &str, lx: i32, rx: i32, y: &mut i32) -> Result<(), Box<dyn std::error::Error>> {
        self.draw_text(d, lx, *y + 15, label, COL_SECTION)?;
        *y += 20;
        self.hline(d, lx, rx - 5, *y - 2, COL_BORDER)?;
        Ok(())
    }

    fn res_stat(&mut self, d: Drawable, label: &str, val: &str, lx: i32, rx: i32, y: i32) -> Result<(), Box<dyn std::error::Error>> {
        self.draw_text(d, lx, y + 15, label, COL_LABEL)?;
        self.draw_text_right(d, rx - 5, y + 15, val, COL_TEXT)?;
        Ok(())
    }

    // ─── Maze rendering ─────────────────────────────────────────────────
    //
    // Same idea as the DIBSection path in the Windows build: fill a plain
    // pixel buffer ourselves, then hand the whole thing to the server in
    // one PutImage instead of issuing a FillRect-equivalent per cell.

    fn render_maze_to_backbuffer(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let snap = match self.last_snapshot.as_ref() { Some(s) => s, None => return Ok(()) };

        let mut is_explored = [[false; 100]; 100];
        for &(r, c) in &snap.explored { is_explored[r as usize][c as usize] = true; }
        let mut is_path = [[false; 100]; 100];
        for &(r, c) in &snap.path { is_path[r as usize][c as usize] = true; }
        let col_exp = if snap.algorithm == Algorithm::Bfs { COL_EXP_BFS } else { COL_EXP_AST };

        let mut buf = vec![COL_WALL; (BUF_W * BUF_H) as usize];
        let fill = |buf: &mut [u32], x0: i32, y0: i32, x1: i32, y1: i32, color: u32| {
            for yy in y0..y1 {
                let row = (yy * BUF_W) as usize;
                for xx in x0..x1 { buf[row + xx as usize] = color; }
            }
        };

        for row in 0..100i32 {
            for col in 0..100i32 {
                let px = X_OFF + col * CELL_PX;
                let py = Y_OFF + row * CELL_PX;
                let color = if row == 0 && col == 0 { COL_START }
                    else if row == 99 && col == 99 { COL_END }
                    else if is_path[row as usize][col as usize] { COL_PATH }
                    else if is_explored[row as usize][col as usize] { col_exp }
                    else { COL_FLOOR };

                fill(&mut buf, px + 1, py + 1, px + 4, py + 4, color);
                let cell = snap.grid[row as usize][col as usize];
                if cell.is_open(Cell::NORTH) && row > 0 { fill(&mut buf, px + 1, py, px + 4, py + 1, color); }
                if cell.is_open(Cell::EAST) && col < 99 { fill(&mut buf, px + 4, py + 1, px + 5, py + 4, color); }
                if cell.is_open(Cell::SOUTH) && row < 99 { fill(&mut buf, px + 1, py + 4, px + 4, py + 5, color); }
                if cell.is_open(Cell::WEST) && col > 0 { fill(&mut buf, px, py + 1, px + 1, py + 4, color); }
            }
        }

        self.put_image_rgb(self.maze_pix, BUF_W, BUF_H, &buf)?;

        let algo_str = match snap.algorithm { Algorithm::Bfs => "BFS", Algorithm::AStar => "A*" };
        let label = format!(" Seed: {:10}  Algo: {} ", snap.seed, algo_str);
        self.fill_rect(self.maze_pix, 0, 0, 220, 16, COL_BTNFACE)?;
        self.draw_text(self.maze_pix, 2, 12, &label, COL_TEXT)?;
        Ok(())
    }

    fn fill_backbuffer(&mut self, pix: Pixmap, w: i32, h: i32, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        self.fill_rect(pix, 0, 0, w, h, color)
    }

    // ─── Low-level GDI-equivalent primitives ─────────────────────────────

    fn fill_rect(&mut self, d: Drawable, x: i32, y: i32, w: i32, h: i32, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        if w <= 0 || h <= 0 { return Ok(()); }
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(color))?;
        self.conn.poly_fill_rectangle(d, self.gc, &[Rectangle { x: x as i16, y: y as i16, width: w as u16, height: h as u16 }])?;
        Ok(())
    }

    fn rect_outline(&mut self, d: Drawable, x: i32, y: i32, w: i32, h: i32, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(color))?;
        self.conn.poly_rectangle(d, self.gc, &[Rectangle { x: x as i16, y: y as i16, width: (w - 1) as u16, height: (h - 1) as u16 }])?;
        Ok(())
    }

    fn hline(&mut self, d: Drawable, x0: i32, x1: i32, y: i32, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(color))?;
        self.conn.poly_line(CoordMode::ORIGIN, d, self.gc, &[Point { x: x0 as i16, y: y as i16 }, Point { x: x1 as i16, y: y as i16 }])?;
        Ok(())
    }

    fn draw_text(&mut self, d: Drawable, x: i32, y: i32, text: &str, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(color).background(COL_WINDOW).font(self.font))?;
        // Core fonts are Latin-1 only; non-ASCII bytes are dropped rather
        // than risking a protocol error on malformed UTF-8 slicing.
        let ascii: Vec<u8> = text.bytes().filter(|b| b.is_ascii()).collect();
        for chunk in ascii.chunks(255) {
            self.conn.image_text8(d, self.gc, x as i16, y as i16, chunk)?;
        }
        Ok(())
    }

    fn draw_text_big(&mut self, d: Drawable, x: i32, y: i32, text: &str, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        // "fixed" has no bold/large variant guaranteed present everywhere,
        // so we fake emphasis by double-struck offset text.
        self.draw_text(d, x + 1, y, text, color)?;
        self.draw_text(d, x, y, text, color)
    }

    fn text_width(&self, text: &str) -> i32 {
        // "fixed" is a 6px-wide monospace core font.
        text.chars().count() as i32 * 6
    }

    fn draw_text_right(&mut self, d: Drawable, right_x: i32, y: i32, text: &str, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.text_width(text);
        self.draw_text(d, right_x - w, y, text, color)
    }

    fn draw_text_centered(&mut self, d: Drawable, center_x: i32, y: i32, text: &str, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.text_width(text);
        self.draw_text(d, center_x - w / 2, y, text, color)
    }

    fn draw_text_big_centered(&mut self, d: Drawable, center_x: i32, y: i32, text: &str, color: u32) -> Result<(), Box<dyn std::error::Error>> {
        let w = self.text_width(text) + 1;
        self.draw_text_big(d, center_x - w / 2, y, text, color)
    }

    fn put_image_rgb(&mut self, pix: Pixmap, w: i32, h: i32, rgb: &[u32]) -> Result<(), Box<dyn std::error::Error>> {
        // Pack into the server's native byte order at our GC's depth. We
        // assume the common 24/32bpp TrueColor layout (RGB in the low 3
        // bytes), which covers effectively every modern Xorg/XWayland
        // default visual.
        let mut data = Vec::with_capacity(rgb.len() * 4);
        for &px in rgb {
            let bytes = if self.byte_order_msb { px.to_be_bytes() } else { px.to_le_bytes() };
            data.extend_from_slice(&bytes);
        }
        // put_image has a request-size ceiling; chunk by row bands to stay
        // safely under it for a 510x510 image.
        let rows_per_chunk = 64usize.max(1);
        let mut y0 = 0i32;
        while y0 < h {
            let rows = rows_per_chunk.min((h - y0) as usize) as i32;
            let start = (y0 * w) as usize * 4;
            let end = ((y0 + rows) * w) as usize * 4;
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                pix, self.gc,
                w as u16, rows as u16,
                0, y0 as i16,
                0, self.depth,
                &data[start..end],
            )?;
            y0 += rows;
        }
        Ok(())
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn mean_u64(v: &[u64]) -> f64 { if v.is_empty() { 0.0 } else { v.iter().sum::<u64>() as f64 / v.len() as f64 } }
fn mean_u32(v: &[u32]) -> f64 { if v.is_empty() { 0.0 } else { v.iter().sum::<u32>() as f64 / v.len() as f64 } }

fn format_score(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let offset = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (i % 3 == offset) { result.push(','); }
        result.push(ch);
    }
    result
}

fn clipboard_text(report: &ScoreReport) -> String {
    let secs = report.total_elapsed_secs as u64;
    let time_str = format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60);
    format!(
        "MazeBench Score: {} ({})\n\
         https://mazebench.mikeden.site/\n\
         \n\
         Performance:\n\
           BFS: {:.0} solves/sec  |  A*: {:.0} solves/sec  |  Combined: {:.0} solves/sec\n\
           Elapsed: {}  |  Threads: {}\n\
         \n\
         Score Breakdown:\n\
           Speed Base:            {}\n\
           Path Optimality:       x {:.3}\n\
           Expl. Efficiency:      x {:.3}\n\
           Consistency:           x {:.3}\n\
           Thermal Sustain:       x {:.3}\n\
           Parallelism Eff:       x {:.3}\n\
           Peak Burst:            x {:.3}\n\
           Algorithm Duel:        x {:.3}\n\
           Correctness:           x {:.3}\n\
           ──────────────────────────────\n\
           FINAL SCORE:           {}\n\
         \n\
         Generated by MazeBench v1.2.2 — https://mazebench.mikeden.site/\n",
        format_score(report.final_score), report.tier,
        report.bfs_solves_per_sec, report.astar_solves_per_sec, report.combined_solves_per_sec,
        time_str, report.num_threads,
        format_score(report.speed_base as u64),
        report.path_optimality_multiplier,
        report.exploration_efficiency_multiplier,
        report.consistency_multiplier,
        report.thermal_sustain_multiplier,
        report.parallelism_efficiency_multiplier,
        report.peak_burst_multiplier,
        report.algorithm_duel_multiplier,
        report.correctness_multiplier,
        format_score(report.final_score),
    )
}
