#![allow(non_snake_case)]
#![allow(unused_must_use)]

use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering::*;
use std::time::Instant;

use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            DataExchange::{OpenClipboard, EmptyClipboard, SetClipboardData, CloseClipboard},
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        },
        UI::{
            Controls::*,
			Input::KeyboardAndMouse::EnableWindow,
            Shell::ShellExecuteW,
            WindowsAndMessaging::*,
        },
    },
};

use crate::benchmark::{self, BFS_TARGET, ASTAR_TARGET, MazeSnapshot, SharedState};
use crate::maze::Cell;
use crate::scoring::{self, ScoreReport};
use crate::solver::Algorithm;

// ─── Constants ────────────────────────────────────────────────────────────────

const ID_BTN_RUN:      u16   = 101;
const ID_BTN_WEBSITE:  u16   = 102;
const ID_BTN_COPY:     u16   = 103;
const ID_PROGRESSBAR:  u16   = 104;
const ID_STATUSBAR:    u16   = 105;
const TIMER_ID_UPDATE: usize = 1001;

/// WM_APP + 1 – posted by the seed-loader thread once seeds are in-hand.
const WM_SEEDS_READY: u32 = 0x8000 + 1;

const CELL_PX: i32 = 5;
const X_OFF:   i32 = 5;
const Y_OFF:   i32 = 5;

/// Pixel dimensions of the maze back-buffer.
const BUF_W: i32 = 510;
const BUF_H: i32 = 510;

// ─── State machine ────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum AppState {
    Ready,
    SeedChecking,   // seeds being fetched on background thread
    Running,
    Done,
}

// ─── Window data stored in GWLP_USERDATA ─────────────────────────────────────

struct WndData {
    shared:       Arc<SharedState>,
    num_threads:  usize,
    state:        AppState,
    // GDI back buffer for maze (510×510) – backed by a DIBSection for
    // direct pixel writes (avoids thousands of FillRect GDI calls/frame).
    hdc_back:     HDC,
    hbmp_back:    HBITMAP,
    // GDI back buffer for the stats panel (390×510) – rendered offscreen
    // then blitted in one shot to eliminate text-flicker.
    hdc_stats:    HDC,
    hbmp_stats:   HBITMAP,
    // GDI back buffer for the results screen (900×546 full-window coords) –
    // same double-buffer trick to stop Score Breakdown from flickering.
    hdc_results:  HDC,
    hbmp_results: HBITMAP,
    /// Raw pointer into the DIBSection pixel data (510*510 × 4 bytes).
    /// Only touched from the UI thread; never sent across threads.
    dib_pixels:   *mut u32,
    // Fonts
    hfont_ui:     HFONT,
    hfont_mono:   HFONT,
    hfont_title:  HFONT,
    hfont_score:  HFONT,
    hfont_score_sm: HFONT,
    // Brushes (kept for GDI text-overlay use)
    hbr_wall:     HBRUSH,
    hbr_floor:    HBRUSH,
    hbr_exp_bfs:  HBRUSH,
    hbr_exp_ast:  HBRUSH,
    hbr_path:     HBRUSH,
    hbr_start:    HBRUSH,
    hbr_end:      HBRUSH,
    hbr_header:   HBRUSH,
    // Child windows
    hwnd_statusbar:  HWND,
    hwnd_btn_run:    HWND,
    hwnd_btn_website:HWND,
    hwnd_btn_copy:   HWND,
    // Results
    score_report:   Option<ScoreReport>,
    last_snapshot:  Option<MazeSnapshot>,
    // Track what we last rendered to avoid redundant back-buffer redraws
    last_rendered_seed: u64,
    last_rendered_algo: Option<crate::solver::Algorithm>,
    // Timing
    bench_start:    Option<Instant>,
    /// Receives seeds from the background loader thread.
    pending_seeds:  Arc<Mutex<Option<Vec<u64>>>>,
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn run(shared: Arc<SharedState>, num_threads: usize) -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;

        let class_name = w!("MazeBenchWnd");

        let wc = WNDCLASSEXW {
            cbSize:        std::mem::size_of::<WNDCLASSEXW>() as u32,
            style:         CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc:   Some(wnd_proc),
            hInstance:     hinstance.into(),
            hCursor:       LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut _),
            lpszClassName: class_name,
            ..Default::default()
        };

        RegisterClassExW(&wc);

        // Calculate window size from client area
        let mut client_rect = RECT { left: 0, top: 0, right: 900, bottom: 638 };
        let style = WS_OVERLAPPEDWINDOW
            & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        AdjustWindowRect(&mut client_rect, style, false)?;

        let win_w = client_rect.right  - client_rect.left;
        let win_h = client_rect.bottom - client_rect.top;

        // Pack shared + num_threads into a heap-allocated tuple for lparam
        let create_data = Box::new((shared, num_threads));
        let lp = Box::into_raw(create_data) as isize;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("MazeBench v1.2.2"),
            style,
            CW_USEDEFAULT, CW_USEDEFAULT,
            win_w, win_h,
            None,
            None,
            hinstance,
            Some(lp as *const _),
        )?;

        // Show debug warning in title if debug build
        #[cfg(debug_assertions)]
        SetWindowTextW(hwnd, w!("MazeBench v1.2.2 [DEBUG BUILD - RESULTS INVALID]"))?;

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

// ─── Window procedure ─────────────────────────────────────────────────────────

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg:  u32,
    wp:   WPARAM,
    lp:   LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE     => on_create(hwnd, lp),
        WM_TIMER      => on_timer(hwnd, wp),
        WM_PAINT      => on_paint(hwnd),
        WM_COMMAND    => on_command(hwnd, wp),
        WM_DESTROY    => on_destroy(hwnd),
        WM_ERASEBKGND => LRESULT(1),  // prevent flicker
        m if m == WM_SEEDS_READY => on_seeds_ready(hwnd),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ─── WM_CREATE ────────────────────────────────────────────────────────────────

unsafe fn on_create(hwnd: HWND, lp: LPARAM) -> LRESULT {
    let cs = &*(lp.0 as *const CREATESTRUCTW);
    let create_data = Box::from_raw(cs.lpCreateParams as *mut (Arc<SharedState>, usize));
    let (shared, num_threads) = *create_data;

    let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap().into();

    // Status bar
    let hwnd_statusbar = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("msctls_statusbar32"),
        None,
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
        0, 0, 0, 0,
        hwnd,
        HMENU(ID_STATUSBAR as *mut _),
        hinstance,
        None,
    ).unwrap();

    // Two-part status bar: [700, -1]
    let parts: [i32; 2] = [700, -1];
    SendMessageW(hwnd_statusbar, SB_SETPARTS,
        WPARAM(2),
        LPARAM(parts.as_ptr() as isize));

    set_status_text(hwnd_statusbar, 0, "Ready");
    set_status_text(hwnd_statusbar, 1, "mazebench.mikeden.site");

    // Run Benchmark button
    let hwnd_btn_run = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("Run Benchmark"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
        10, 578, 160, 30,
        hwnd,
        HMENU(ID_BTN_RUN as *mut _),
        hinstance,
        None,
    ).unwrap();

    // Visit Website button
    let hwnd_btn_website = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("Visit Website"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
        180, 578, 160, 30,
        hwnd,
        HMENU(ID_BTN_WEBSITE as *mut _),
        hinstance,
        None,
    ).unwrap();

    // Copy Results button (hidden)
    let hwnd_btn_copy = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("Copy Results"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
        350, 578, 160, 30,
        hwnd,
        HMENU(ID_BTN_COPY as *mut _),
        hinstance,
        None,
    ).unwrap();
    ShowWindow(hwnd_btn_copy, SW_HIDE);

    // GDI back buffer for maze – DIBSection lets us write pixels directly,
    // eliminating thousands of FillRect GDI calls per frame.
    let hdc_screen = GetDC(hwnd);
    let hdc_back   = CreateCompatibleDC(hdc_screen);

    let bmi = windows::Win32::Graphics::Gdi::BITMAPINFO {
        bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
            biSize:        std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
            biWidth:       BUF_W,
            biHeight:      -BUF_H,  // negative = top-down scan order
            biPlanes:      1,
            biBitCount:    32,
            biCompression: 0,       // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let mut dib_bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbmp_back = windows::Win32::Graphics::Gdi::CreateDIBSection(
        hdc_screen,
        &bmi,
        windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
        &mut dib_bits,
        HANDLE::default(),
        0,
    ).expect("CreateDIBSection failed");
    let dib_pixels = dib_bits as *mut u32;
    SelectObject(hdc_back, hbmp_back);

    // Stats panel back-buffer: 390 wide (900-510), 510 tall – same height as maze.
    let stats_w = 390i32;
    let stats_h = BUF_H;
    let hdc_stats  = CreateCompatibleDC(hdc_screen);
    let hbmp_stats = CreateCompatibleBitmap(hdc_screen, stats_w, stats_h);
    SelectObject(hdc_stats, hbmp_stats);

    // Results screen back-buffer: full window dimensions so coords are unchanged.
    let hdc_results  = CreateCompatibleDC(hdc_screen);
    let hbmp_results = CreateCompatibleBitmap(hdc_screen, 900, 546);
    SelectObject(hdc_results, hbmp_results);

    ReleaseDC(hwnd, hdc_screen);

    // Pre-fill with wall colour so the back-buffer isn't garbage.
    std::slice::from_raw_parts_mut(dib_pixels, (BUF_W * BUF_H) as usize)
        .fill(0x001A1A1A);

    let default_font = w!("Segoe UI");

    // Fonts
    let hfont_ui    = make_font(16, FW_NORMAL.0, false, default_font);
    let hfont_mono  = make_font(14, FW_NORMAL.0, false, w!("Courier New"));
    let hfont_title = make_font(16, FW_BOLD.0,   false, default_font);
    let hfont_score = make_font(36, FW_BOLD.0,   false, w!("Courier New"));
    let hfont_score_sm = make_font(18, FW_BOLD.0, false, default_font);

    // Brushes
    let hbr_wall    = CreateSolidBrush(COLORREF(0x001A1A1A));
    let hbr_floor   = CreateSolidBrush(COLORREF(0x00F0F0F0));
    let hbr_exp_bfs = CreateSolidBrush(COLORREF(0x00C8D8F0));
    let hbr_exp_ast = CreateSolidBrush(COLORREF(0x00C8E8F0));
    let hbr_path    = CreateSolidBrush(COLORREF(0x000050C8));
    let hbr_start   = CreateSolidBrush(COLORREF(0x0000A000));
    let hbr_end     = CreateSolidBrush(COLORREF(0x00C00000));
    let hbr_header  = CreateSolidBrush(COLORREF(0x00F0F0F0));

    let data = Box::new(WndData {
        shared,
        num_threads,
        state: AppState::Ready,
        hdc_back,
        hbmp_back,
        hdc_stats,
        hbmp_stats,
        hdc_results,
        hbmp_results,
        dib_pixels,
        hfont_ui,
        hfont_mono,
        hfont_title,
        hfont_score,
        hfont_score_sm,
        hbr_wall,
        hbr_floor,
        hbr_exp_bfs,
        hbr_exp_ast,
        hbr_path,
        hbr_start,
        hbr_end,
        hbr_header,
        hwnd_statusbar,
        hwnd_btn_run,
        hwnd_btn_website,
        hwnd_btn_copy,
        score_report: None,
        last_snapshot: None,
        last_rendered_seed: 0,
        last_rendered_algo: None,
        bench_start: None,
        pending_seeds: Arc::new(Mutex::new(None)),
    });

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);
    // 16 ms timer ≈ 60 fps; keeps the preview smooth without burning CPU.
    SetTimer(hwnd, TIMER_ID_UPDATE, 16, None);

    LRESULT(0)
}

// ─── WM_TIMER ─────────────────────────────────────────────────────────────────

unsafe fn on_timer(hwnd: HWND, wp: WPARAM) -> LRESULT {
    if wp.0 != TIMER_ID_UPDATE { return LRESULT(0); }

    let data = get_data(hwnd);
    // SeedChecking: UI update happens via WM_SEEDS_READY instead.
    if data.state == AppState::Ready || data.state == AppState::SeedChecking {
        return LRESULT(0);
    }

    let bfs   = data.shared.bfs_completed.load(Relaxed);
    let astar = data.shared.astar_completed.load(Relaxed);

    // Update progress bar
    let bar_rect = RECT { left: 0, top: 546, right: 900, bottom: 574 };
    InvalidateRect(hwnd, Some(&bar_rect), FALSE);

    // Update status bar
    let elapsed = data.bench_start
        .as_ref()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let secs = elapsed as u64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    let status = format!(
        "BFS: {:>9} / 400,000  |  A*: {:>12} / 1,200,000  |  {:02}:{:02}:{:02}",
        bfs, astar, h, m, s
    );
    set_status_text(data.hwnd_statusbar, 0, &status);

    // Pull the latest snapshot the worker thread posted (non-blocking).
    if let Ok(mut snap_lock) = data.shared.latest_snapshot.try_lock() {
        if let Some(snap) = snap_lock.take() {
            data.last_snapshot = Some(snap);
        }
    }

    // Only re-render the back-buffer when the snapshot actually changed.
    // This avoids thousands of pixel-write operations when the benchmark
    // produces frames faster than our 16 ms timer tick.
    if let Some(ref snap) = data.last_snapshot {
        let seed_changed = snap.seed != data.last_rendered_seed;
        let algo_changed = data.last_rendered_algo
            .map(|a| a != snap.algorithm)
            .unwrap_or(true);

        if seed_changed || algo_changed {
            data.last_rendered_seed = snap.seed;
            data.last_rendered_algo = Some(snap.algorithm);
            render_maze_to_backbuffer(data);
            let maze_rect = RECT { left: 0, top: 36, right: BUF_W, bottom: 36 + BUF_H };
            InvalidateRect(hwnd, Some(&maze_rect), FALSE);
        }
    }

    // Repaint stats panel every tick (it shows live numbers).
    // Use FALSE (no erase) because draw_stats_panel fills its own background
    // with FillRect; erasing here causes the text to flash on every tick.
    let stats_rect = RECT { left: BUF_W, top: 36, right: 900, bottom: 36 + BUF_H };
    InvalidateRect(hwnd, Some(&stats_rect), FALSE);

    // Check if benchmark finished naturally
    if data.shared.done.load(Acquire) && data.state == AppState::Running {
        KillTimer(hwnd, TIMER_ID_UPDATE);

        let report = scoring::compute(&data.shared, data.num_threads);

        let score_str = format!("Score: {}", format_score(report.final_score));
        set_status_text(data.hwnd_statusbar, 0, "Benchmark complete!");
        set_status_text(data.hwnd_statusbar, 1, &score_str);

        data.score_report = Some(report);
        data.state = AppState::Done;

        // Replace "Stop Benchmark" with "Run Benchmark" text, then hide it
        // and show Copy Results.
        SetWindowTextW(data.hwnd_btn_run, w!("Run Benchmark")).ok();
        ShowWindow(data.hwnd_btn_run, SW_HIDE);
        ShowWindow(data.hwnd_btn_copy, SW_SHOW);

        SetWindowTextW(hwnd, w!("MazeBench v1.2.2 — Done")).ok();
        InvalidateRect(hwnd, None, TRUE);
    }

    LRESULT(0)
}

// ─── WM_PAINT ─────────────────────────────────────────────────────────────────

unsafe fn on_paint(hwnd: HWND) -> LRESULT {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let data = get_data(hwnd);

    // Header panel (y=0, h=36)
    let header_rect = RECT { left: 0, top: 0, right: 900, bottom: 36 };
    FillRect(hdc, &header_rect, data.hbr_header);

    SelectObject(hdc, data.hfont_title);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00800000));

    let mut title_rect = RECT { left: 10, top: 8, right: 500, bottom: 36 };
    draw_text_w(hdc, "MazeBench v1.2.2", &mut title_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    SetTextColor(hdc, COLORREF(0x00604000));
    let mut link_rect = RECT { left: 400, top: 8, right: 890, bottom: 36 };
    draw_text_w(hdc, "mazebench.mikeden.site", &mut link_rect, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    // Separator line under header
    let old_pen = SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCPenColor(hdc, COLORREF(0x00CCCCCC));
    MoveToEx(hdc, 0, 35, None);
    LineTo(hdc, 900, 35);
    SelectObject(hdc, old_pen);

    match data.state {
        AppState::Done => {
            if let Some(ref report) = data.score_report {
                // Render into back-buffer then blit atomically – eliminates
                // the Score Breakdown flicker caused by per-glyph screen writes.
                draw_results_screen(data.hdc_results, data, report);
                BitBlt(hdc, 0, 36, 900, 510, data.hdc_results, 0, 36, SRCCOPY).ok();
            }
        }
        _ => {
            // Blit maze back buffer
            BitBlt(hdc, 0, 36, 510, 510, data.hdc_back, 0, 0, SRCCOPY).ok();

            // Sunken border around maze
            let mut maze_border = RECT { left: 0, top: 36, right: 510, bottom: 546 };
            DrawEdge(hdc, &mut maze_border, EDGE_SUNKEN, BF_RECT);

            // Draw stats panel into its back-buffer, then blit in one shot
            // to eliminate GDI text flicker.
            draw_stats_panel(data.hdc_stats, data);
            BitBlt(hdc, BUF_W, 36, 390, 510, data.hdc_stats, 0, 0, SRCCOPY).ok();
        }
    }

    // Draw custom progress bar
    let bfs   = data.shared.bfs_completed.load(Relaxed);
    let astar = data.shared.astar_completed.load(Relaxed);
    let target = (BFS_TARGET + ASTAR_TARGET) as u64;
    let total = (bfs + astar).min(target);
    
    let pct = total as f64 / target as f64;
    let mut bar_rect = RECT { left: 0, top: 546, right: 900, bottom: 574 };
    
    // Background
    FillRect(hdc, &bar_rect, GetSysColorBrush(SYS_COLOR_INDEX(COLOR_WINDOW.0)));
    
    // Filled
    let mut fill_rect = bar_rect;
    fill_rect.right = (900.0 * pct) as i32;
    FillRect(hdc, &fill_rect, data.hbr_path);
    
    // DrawEdge sunken
    DrawEdge(hdc, &mut bar_rect, EDGE_SUNKEN, BF_RECT);
    
    // Text
    let pct_str = format!("{:.2}%", pct * 100.0);
    SelectObject(hdc, data.hfont_ui);
    SetBkMode(hdc, TRANSPARENT);
    
    // Draw white text over the filled part
    let hrgn_fill = CreateRectRgn(fill_rect.left, fill_rect.top, fill_rect.right, fill_rect.bottom);
    SelectClipRgn(hdc, hrgn_fill);
    SetTextColor(hdc, COLORREF(0x00FFFFFF)); // White
    draw_text_w(hdc, &pct_str, &mut bar_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    
    // Draw black text over the unfilled part
    let hrgn_empty = CreateRectRgn(fill_rect.right, bar_rect.top, bar_rect.right, bar_rect.bottom);
    SelectClipRgn(hdc, hrgn_empty);
    SetTextColor(hdc, COLORREF(0x00000000)); // Black
    draw_text_w(hdc, &pct_str, &mut bar_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    
    // Remove clip
    SelectClipRgn(hdc, HRGN::default());
    DeleteObject(hrgn_fill);
    DeleteObject(hrgn_empty);

    EndPaint(hwnd, &ps);
    LRESULT(0)
}

// ─── Stats panel ──────────────────────────────────────────────────────────────

unsafe fn draw_stats_panel(hdc: HDC, data: &WndData) {
    let bfs   = data.shared.bfs_completed.load(Relaxed);
    let astar = data.shared.astar_completed.load(Relaxed);

    let elapsed = data.bench_start
        .as_ref()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0)
        .max(0.001);

    let secs = elapsed as u64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    let bfs_per_sec   = bfs   as f64 / elapsed;
    let astar_per_sec = astar as f64 / elapsed;
    let combined_per_sec = (bfs + astar) as f64 / elapsed;

    // ETA
    let progress = (bfs + astar) as f64 / (BFS_TARGET + ASTAR_TARGET) as f64;
    let eta_str = if progress > 0.01 {
        let total_est = elapsed / progress;
        let remaining = (total_est - elapsed).max(0.0) as u64;
        let rh = remaining / 3600;
        let rm = (remaining % 3600) / 60;
        let rs = remaining % 60;
        format!("{:02}:{:02}:{:02}", rh, rm, rs)
    } else {
        "??:??:??".to_string()
    };

    // Average times (from locked data, try_lock)
    let avg_bfs_ms = if let Ok(v) = data.shared.bfs_times_ns.try_lock() {
        mean_u64(&v) / 1_000_000.0
    } else { 0.0 };

    let avg_astar_ms = if let Ok(v) = data.shared.astar_times_ns.try_lock() {
        mean_u64(&v) / 1_000_000.0
    } else { 0.0 };

    let avg_bfs_exp = if let Ok(v) = data.shared.bfs_explored.try_lock() {
        mean_u32(&v)
    } else { 0.0 };

    let avg_astar_exp = if let Ok(v) = data.shared.astar_explored.try_lock() {
        mean_u32(&v)
    } else { 0.0 };

    let efficiency = if avg_bfs_exp > 0.0 {
        (1.0 - (avg_astar_exp / avg_bfs_exp).min(1.0)) * 100.0
    } else { 0.0 };

    let wrong = data.shared.wrong_solutions.load(Relaxed);

    // Live score estimate
    // Only show once there is a stable reading (≥3 s elapsed, ≥200 solves).
    // The cumulative rate (bfs+astar)/elapsed spikes in the first few seconds
    // because thread-startup overhead briefly inflates apparent throughput.
    let show_live = elapsed >= 3.0 && (bfs + astar) >= 200;

    // Draw panel background – coords are local to the offscreen DC (origin = panel top-left).
    // Screen panel spans x 510-900, y 36-546; here everything is offset by (-510, -36).
    let panel_rect = RECT { left: 0, top: 0, right: 390, bottom: 510 };
    FillRect(hdc, &panel_rect, GetSysColorBrush(SYS_COLOR_INDEX(COLOR_WINDOW.0)));

    SetBkMode(hdc, TRANSPARENT);

    // x=518 on screen -> 8 in local coords; y=44 on screen -> 8 in local coords.
    // right=895 on screen -> 385 in local coords; value col 700 -> 190.
    let x = 8i32;
    let mut y = 8i32;
    let line_h = 19i32;

    macro_rules! section {
        ($label:expr) => {{
            SelectObject(hdc, data.hfont_title);
            SetTextColor(hdc, COLORREF(0x00404040));
            let mut r = RECT { left: x, top: y, right: 385, bottom: y + line_h };
            draw_text_w(hdc, $label, &mut r, DT_LEFT | DT_SINGLELINE);
            y += line_h;
            // underline
            let op = SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCPenColor(hdc, COLORREF(0x00CCCCCC));
            MoveToEx(hdc, x, y - 2, None);
            LineTo(hdc, 385, y - 2);
            SelectObject(hdc, op);
            y += 2;
        }};
    }

    macro_rules! stat_line {
        ($label:expr, $val:expr) => {{
            SelectObject(hdc, data.hfont_ui);
            SetTextColor(hdc, COLORREF(0x00505050));
            let mut lr = RECT { left: x, top: y, right: 190, bottom: y + line_h };
            draw_text_w(hdc, $label, &mut lr, DT_LEFT | DT_SINGLELINE);

            SelectObject(hdc, data.hfont_mono);
            SetTextColor(hdc, COLORREF(0x00000000));
            let mut vr = RECT { left: 190, top: y, right: 385, bottom: y + line_h };
            draw_text_w(hdc, &$val, &mut vr, DT_RIGHT | DT_SINGLELINE);
            y += line_h;
        }};
    }

    section!("Benchmark Progress");
    stat_line!("BFS Solves",  format!("{:>9} / 400,000", bfs));
    stat_line!("A* Solves",   format!("{:>12} / 1,200,000", astar));
    y += 4;

    section!("Throughput");
    stat_line!("BFS",      format!("{:>8.0} solves/sec", bfs_per_sec));
    stat_line!("A*",       format!("{:>8.0} solves/sec", astar_per_sec));
    stat_line!("Combined", format!("{:>8.0} solves/sec", combined_per_sec));
    y += 4;

    section!("Timing");
    stat_line!("Elapsed", format!("{:02}:{:02}:{:02}", h, m, s));
    stat_line!("ETA",     eta_str);
    stat_line!("Avg BFS", format!("{:.3} ms/solve", avg_bfs_ms));
    stat_line!("Avg A*",  format!("{:.3} ms/solve", avg_astar_ms));
    y += 4;

    section!("Exploration");
    stat_line!("BFS avg explored", format!("{:>6.0} cells", avg_bfs_exp));
    stat_line!("A* avg explored",  format!("{:>6.0} cells", avg_astar_exp));
    stat_line!("A* efficiency",    format!("{:>5.1}%", efficiency));
    y += 4;

    section!("System");
    stat_line!("CPU threads",  format!("{}", data.num_threads));
    stat_line!("Wrong solves", format!("{}", wrong));
    y += 4;

    // Live score estimate – big centered text
    section!("Live Score Estimate");
    if !show_live {
        SelectObject(hdc, data.hfont_score_sm);
        SetTextColor(hdc, COLORREF(0x00808080));
        let mut wr = RECT { left: x, top: y + 6, right: 385, bottom: y + 30 };
        draw_text_w(hdc, "Warming up...", &mut wr, DT_CENTER | DT_SINGLELINE);
    } else {
        let live_score  = (combined_per_sec * 1_000.0).round() as u64;
        let score_str   = format_score(live_score);
        let tier_label  = crate::scoring::tier_for(live_score);

        SelectObject(hdc, data.hfont_score);
        SetTextColor(hdc, COLORREF(0x00003080));
        let mut sr = RECT { left: x, top: y + 4, right: 385, bottom: y + 44 };
        draw_text_w(hdc, &score_str, &mut sr, DT_CENTER | DT_SINGLELINE);
        y += 48;

        SelectObject(hdc, data.hfont_score_sm);
        SetTextColor(hdc, tier_color(tier_label));
        let mut tr = RECT { left: x, top: y, right: 385, bottom: y + 24 };
        draw_text_w(hdc, tier_label, &mut tr, DT_CENTER | DT_SINGLELINE);
    }
}

// ─── Results screen ───────────────────────────────────────────────────────────

unsafe fn draw_results_screen(hdc: HDC, data: &WndData, report: &ScoreReport) {
    // Background
    let bg = RECT { left: 0, top: 36, right: 900, bottom: 546 };
    FillRect(hdc, &bg, GetSysColorBrush(SYS_COLOR_INDEX(COLOR_WINDOW.0)));

    // ── Score box ──────────────────────────────────────────────────────────
    let mut score_box = RECT { left: 20, top: 46, right: 880, bottom: 156 };
    DrawEdge(hdc, &mut score_box, EDGE_SUNKEN, BF_RECT);

    SelectObject(hdc, data.hfont_score);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00003080));
    let score_str = format_score(report.final_score);
    let mut sr = RECT { left: 30, top: 58, right: 870, bottom: 110 };
    draw_text_w(hdc, &score_str, &mut sr, DT_CENTER | DT_SINGLELINE);

    SelectObject(hdc, data.hfont_score_sm);
    SetTextColor(hdc, tier_color(report.tier));
    let mut tr = RECT { left: 30, top: 114, right: 870, bottom: 150 };
    draw_text_w(hdc, report.tier, &mut tr, DT_CENTER | DT_SINGLELINE);

    // ── Performance box (left) ─────────────────────────────────────────────
    let mut perf_box = RECT { left: 20, top: 166, right: 440, bottom: 540 };
    DrawEdge(hdc, &mut perf_box, EDGE_SUNKEN, BF_RECT);

    let lx = 30i32;
    let rx = 435i32;
    let mut ly = 174i32;
    let lh = 20i32;

    macro_rules! result_header {
        ($label:expr, $y:expr) => {{
            SelectObject(hdc, data.hfont_title);
            SetTextColor(hdc, COLORREF(0x00404040));
            let mut r = RECT { left: lx, top: $y, right: rx, bottom: $y + lh };
            draw_text_w(hdc, $label, &mut r, DT_LEFT | DT_SINGLELINE);
            $y += lh;
            let op = SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCPenColor(hdc, COLORREF(0x00CCCCCC));
            MoveToEx(hdc, lx, $y - 2, None);
            LineTo(hdc, rx - 5, $y - 2);
            SelectObject(hdc, op);
            $y += 2;
        }};
    }

    macro_rules! result_stat {
        ($label:expr, $val:expr, $y:expr) => {{
            SelectObject(hdc, data.hfont_ui);
            SetTextColor(hdc, COLORREF(0x00505050));
            let mut lr = RECT { left: lx, top: $y, right: lx + 130, bottom: $y + lh };
            draw_text_w(hdc, $label, &mut lr, DT_LEFT | DT_SINGLELINE);
            SelectObject(hdc, data.hfont_mono);
            SetTextColor(hdc, COLORREF(0x00000000));
            let mut vr = RECT { left: lx + 130, top: $y, right: rx - 5, bottom: $y + lh };
            draw_text_w(hdc, &$val, &mut vr, DT_RIGHT | DT_SINGLELINE);
            $y += lh;
        }};
    }

    result_header!("Performance", ly);
    result_stat!("BFS",      format!("{:.0} solves/sec", report.bfs_solves_per_sec), ly);
    result_stat!("A*",       format!("{:.0} solves/sec", report.astar_solves_per_sec), ly);
    result_stat!("Combined", format!("{:.0} solves/sec", report.combined_solves_per_sec), ly);

    let secs = report.total_elapsed_secs as u64;
    result_stat!("Elapsed",  format!("{:02}:{:02}:{:02}", secs/3600, (secs%3600)/60, secs%60), ly);
    result_stat!("Threads",  format!("{}", report.num_threads), ly);
    ly += 6;

    result_header!("Quality", ly);
    result_stat!("Avg BFS",    format!("{:.3} ms/solve", report.avg_bfs_time_us / 1000.0), ly);
    result_stat!("Avg A*",     format!("{:.3} ms/solve", report.avg_astar_time_us / 1000.0), ly);
    result_stat!("BFS cells",  format!("{:.0} avg explored", report.avg_bfs_explored), ly);
    result_stat!("A* cells",   format!("{:.0} avg explored", report.avg_astar_explored), ly);
    result_stat!("Gen time",   format!("{:.3} ms/maze", report.avg_gen_time_us / 1000.0), ly);
    result_stat!("Wrong",      format!("{}", report.wrong_count), ly);

    // ── Score breakdown box (right) ────────────────────────────────────────
    let mut brk_box = RECT { left: 460, top: 166, right: 880, bottom: 540 };
    DrawEdge(hdc, &mut brk_box, EDGE_SUNKEN, BF_RECT);

    let bx = 470i32;
    let bxr = 875i32;
    let mut by_ = 174i32;
    let bh = 20i32;

    macro_rules! brk_header {
        ($label:expr) => {{
            SelectObject(hdc, data.hfont_title);
            SetTextColor(hdc, COLORREF(0x00404040));
            let mut r = RECT { left: bx, top: by_, right: bxr, bottom: by_ + bh };
            draw_text_w(hdc, $label, &mut r, DT_LEFT | DT_SINGLELINE);
            by_ += bh;
            let op = SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCPenColor(hdc, COLORREF(0x00CCCCCC));
            MoveToEx(hdc, bx, by_ - 2, None);
            LineTo(hdc, bxr - 5, by_ - 2);
            SelectObject(hdc, op);
            by_ += 2;
        }};
    }

    macro_rules! brk_line {
        ($label:expr, $val_str:expr, $mult:expr) => {{
            // Label
            SelectObject(hdc, data.hfont_ui);
            SetTextColor(hdc, COLORREF(0x00505050));
            let mut lr = RECT { left: bx, top: by_, right: bx + 160, bottom: by_ + bh };
            draw_text_w(hdc, $label, &mut lr, DT_LEFT | DT_SINGLELINE);

            // Value
            SelectObject(hdc, data.hfont_mono);
            SetTextColor(hdc, COLORREF(0x00000000));
            let mut vr = RECT { left: bx + 160, top: by_, right: bxr - 25, bottom: by_ + bh };
            draw_text_w(hdc, $val_str, &mut vr, DT_RIGHT | DT_SINGLELINE);

            // Symbol
            let sym   = mult_sym($mult);
            let color = mult_color($mult);
            SetTextColor(hdc, color);
            SelectObject(hdc, data.hfont_title);
            let mut symr = RECT { left: bxr - 22, top: by_, right: bxr, bottom: by_ + bh };
            draw_text_w(hdc, sym, &mut symr, DT_CENTER | DT_SINGLELINE);

            by_ += bh;
        }};
    }

    brk_header!("Score Breakdown");

    // Speed base (no multiplier symbol)
    SelectObject(hdc, data.hfont_ui);
    SetTextColor(hdc, COLORREF(0x00505050));
    let mut slr = RECT { left: bx, top: by_, right: bx + 160, bottom: by_ + bh };
    draw_text_w(hdc, "Speed Base", &mut slr, DT_LEFT | DT_SINGLELINE);
    SelectObject(hdc, data.hfont_mono);
    SetTextColor(hdc, COLORREF(0x00000000));
    let mut svr = RECT { left: bx + 160, top: by_, right: bxr - 5, bottom: by_ + bh };
    draw_text_w(hdc, &format_score(report.speed_base as u64), &mut svr, DT_RIGHT | DT_SINGLELINE);
    by_ += bh;

    brk_line!("Path Optimality",   &format!("× {:.3}", report.path_optimality_multiplier),        report.path_optimality_multiplier);
    brk_line!("Expl. Efficiency",  &format!("× {:.3}", report.exploration_efficiency_multiplier),  report.exploration_efficiency_multiplier);
    brk_line!("Consistency",       &format!("× {:.3}", report.consistency_multiplier),             report.consistency_multiplier);
    brk_line!("Thermal Sustain",   &format!("× {:.3}", report.thermal_sustain_multiplier),         report.thermal_sustain_multiplier);
    brk_line!("Parallelism Eff",   &format!("× {:.3}", report.parallelism_efficiency_multiplier),  report.parallelism_efficiency_multiplier);
    brk_line!("Peak Burst",        &format!("× {:.3}", report.peak_burst_multiplier),              report.peak_burst_multiplier);
    brk_line!("Algorithm Duel",    &format!("× {:.3}", report.algorithm_duel_multiplier),          report.algorithm_duel_multiplier);
    brk_line!("Correctness",       &format!("× {:.3}", report.correctness_multiplier),             report.correctness_multiplier);

    // Divider + final score
    by_ += 4;
    let op = SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCPenColor(hdc, COLORREF(0x00888888));
    MoveToEx(hdc, bx, by_, None);
    LineTo(hdc, bxr - 5, by_);
    SelectObject(hdc, op);
    by_ += 6;

    SelectObject(hdc, data.hfont_title);
    SetTextColor(hdc, COLORREF(0x00003080));
    let mut flr = RECT { left: bx, top: by_, right: bx + 160, bottom: by_ + bh };
    draw_text_w(hdc, "FINAL SCORE", &mut flr, DT_LEFT | DT_SINGLELINE);
    SelectObject(hdc, data.hfont_mono);
    let mut fvr = RECT { left: bx + 160, top: by_, right: bxr - 5, bottom: by_ + bh };
    draw_text_w(hdc, &format_score(report.final_score), &mut fvr, DT_RIGHT | DT_SINGLELINE);
}

// ─── WM_COMMAND ───────────────────────────────────────────────────────────────

unsafe fn on_command(hwnd: HWND, wp: WPARAM) -> LRESULT {
    let id = (wp.0 & 0xFFFF) as u16;
    let data = get_data(hwnd);

    match id {
        ID_BTN_RUN => {
			match data.state {
				AppState::Ready => {
				set_status_text(data.hwnd_statusbar, 0, "Checking seeds...");
				SetWindowTextW(data.hwnd_btn_run, w!("Checking seeds...")).ok();
				EnableWindow(data.hwnd_btn_run, FALSE);
				data.state = AppState::SeedChecking;

				let slot = Arc::clone(&data.pending_seeds);
            
				// FIX: Cast the raw pointer to an isize to satisfy Send
				let hwnd_raw = hwnd.0 as isize; 
            
				std::thread::spawn(move || {
					let seeds = crate::maze::load_seeds();
					*slot.lock().unwrap() = Some(seeds);
					
					// FIX: Reconstruct the HWND inside the thread
					let hwnd_target = HWND(hwnd_raw as *mut _);
					let _ = PostMessageW(hwnd_target, WM_SEEDS_READY, WPARAM(0), LPARAM(0));
				});
			}
                AppState::Running => {
                    // ── Stop: signal workers to exit, transition to Done ────
                    data.shared.done.store(true, std::sync::atomic::Ordering::Release);
                    set_status_text(data.hwnd_statusbar, 0, "Benchmark stopped.");
                    SetWindowTextW(data.hwnd_btn_run, w!("Run Benchmark")).ok();
                    ShowWindow(data.hwnd_btn_run, SW_HIDE);
                    ShowWindow(data.hwnd_btn_copy, SW_SHOW);

                    // Compute whatever partial results we have.
                    let report = scoring::compute(&data.shared, data.num_threads);
                    let score_str = format!("Score: {} (partial)", format_score(report.final_score));
                    set_status_text(data.hwnd_statusbar, 1, &score_str);
                    data.score_report = Some(report);
                    data.state = AppState::Done;

                    SetWindowTextW(hwnd, w!("MazeBench v1.2.2 — Stopped")).ok();
                    InvalidateRect(hwnd, None, TRUE);
                }
                _ => {}
            }
				}
        ID_BTN_WEBSITE => {
            ShellExecuteW(
                hwnd,
                w!("open"),
                w!("https://mazebench.mikeden.site/"),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
        ID_BTN_COPY => {
            if let Some(ref report) = data.score_report {
                copy_to_clipboard(hwnd, report);
            }
        }
        _ => {}
    }

    LRESULT(0)
}

// ─── WM_SEEDS_READY ───────────────────────────────────────────────────────────

unsafe fn on_seeds_ready(hwnd: HWND) -> LRESULT {
    let data = get_data(hwnd);

    // Retrieve seeds that the loader thread stored.
    let seeds = {
        let mut slot = data.pending_seeds.lock().unwrap();
        slot.take().unwrap_or_else(|| vec![crate::maze::FALLBACK_SEED])
    };

    // Re-enable the button now that we have seeds; text → "Stop Benchmark"
    // so the user can abort mid-run.
    EnableWindow(data.hwnd_btn_run, TRUE);
    SetWindowTextW(data.hwnd_btn_run, w!("Stop Benchmark")).ok();

    set_status_text(data.hwnd_statusbar, 0, "Benchmark running...");
    data.state     = AppState::Running;
    data.bench_start = Some(Instant::now());

    benchmark::start(Arc::clone(&data.shared), data.num_threads, Arc::new(seeds));

    LRESULT(0)
}

// ─── WM_DESTROY ───────────────────────────────────────────────────────────────

unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    KillTimer(hwnd, TIMER_ID_UPDATE);
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndData;
    if !ptr.is_null() {
        let data = Box::from_raw(ptr);
        DeleteDC(data.hdc_back);
        DeleteObject(data.hbmp_back);
        DeleteDC(data.hdc_stats);
        DeleteObject(data.hbmp_stats);
        DeleteDC(data.hdc_results);
        DeleteObject(data.hbmp_results);
        DeleteObject(data.hfont_ui);
        DeleteObject(data.hfont_mono);
        DeleteObject(data.hfont_title);
        DeleteObject(data.hfont_score);
        DeleteObject(data.hfont_score_sm);
        DeleteObject(data.hbr_wall);
        DeleteObject(data.hbr_floor);
        DeleteObject(data.hbr_exp_bfs);
        DeleteObject(data.hbr_exp_ast);
        DeleteObject(data.hbr_path);
        DeleteObject(data.hbr_start);
        DeleteObject(data.hbr_end);
        DeleteObject(data.hbr_header);
        // data drops: Arc<SharedState> ref count decrements
    }
    PostQuitMessage(0);
    LRESULT(0)
}

// ─── Maze rendering ───────────────────────────────────────────────────────────
//
// We write directly into the DIBSection pixel buffer instead of calling
// FillRect for every cell.  The old approach issued 10 000+ GDI calls per
// frame; this version does a handful of pointer writes per cell and is
// several orders of magnitude faster, keeping the preview smooth.

/// Convert a COLORREF (0x00BBGGRR) to the little-endian u32 that a 32-bpp
/// top-down DIBSection expects  (byte order in memory: B G R 00).
#[inline]
const fn cr(colorref: u32) -> u32 {
    let r = colorref & 0xFF;
    let g = (colorref >> 8) & 0xFF;
    let b = (colorref >> 16) & 0xFF;
    b | (g << 8) | (r << 16)
}

// Pre-computed DIB pixel colours that match the COLORREF values used by the
// original GDI brushes.
const PIX_WALL:    u32 = cr(0x001A1A1A);
const PIX_FLOOR:   u32 = cr(0x00F0F0F0);
const PIX_EXP_BFS: u32 = cr(0x00C8D8F0);
const PIX_EXP_AST: u32 = cr(0x00C8E8F0);
const PIX_PATH:    u32 = cr(0x000050C8);
const PIX_START:   u32 = cr(0x0000A000);
const PIX_END:     u32 = cr(0x00C00000);

/// Fill a rectangle inside the 510-wide DIB with a single colour.
#[inline]
unsafe fn dib_fill(pixels: *mut u32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    for y in y0..y1 {
        let row = pixels.add((y * BUF_W + x0) as usize);
        for x in 0..(x1 - x0) {
            *row.add(x as usize) = color;
        }
    }
}

unsafe fn render_maze_to_backbuffer(data: &WndData) {
    let snap = match data.last_snapshot.as_ref() {
        Some(s) => s,
        None    => return,
    };

    let pixels = data.dib_pixels;

    // ── Clear to wall colour ──────────────────────────────────────────────
    std::slice::from_raw_parts_mut(pixels, (BUF_W * BUF_H) as usize)
        .fill(PIX_WALL);

    // ── Build lookup tables ───────────────────────────────────────────────
    let mut is_explored = [[false; 100]; 100];
    for &(r, c) in &snap.explored {
        is_explored[r as usize][c as usize] = true;
    }
    let mut is_path = [[false; 100]; 100];
    for &(r, c) in &snap.path {
        is_path[r as usize][c as usize] = true;
    }

    let pix_exp = if snap.algorithm == Algorithm::Bfs { PIX_EXP_BFS } else { PIX_EXP_AST };

    // ── Paint cells ───────────────────────────────────────────────────────
    for row in 0..100usize {
        for col in 0..100usize {
            let px = X_OFF + col as i32 * CELL_PX;
            let py = Y_OFF + row as i32 * CELL_PX;

            let color = if row == 0 && col == 0 {
                PIX_START
            } else if row == 99 && col == 99 {
                PIX_END
            } else if is_path[row][col] {
                PIX_PATH
            } else if is_explored[row][col] {
                pix_exp
            } else {
                PIX_FLOOR
            };

            // 3×3 cell interior
            dib_fill(pixels, px+1, py+1, px+4, py+4, color);

            let cell = snap.grid[row][col];

            // Open passage pixels (1-px wide strips between cells)
            if cell.is_open(Cell::NORTH) && row > 0 {
                dib_fill(pixels, px+1, py,   px+4, py+1, color);
            }
            if cell.is_open(Cell::EAST) && col < 99 {
                dib_fill(pixels, px+4, py+1, px+5, py+4, color);
            }
            if cell.is_open(Cell::SOUTH) && row < 99 {
                dib_fill(pixels, px+1, py+4, px+4, py+5, color);
            }
            if cell.is_open(Cell::WEST) && col > 0 {
                dib_fill(pixels, px,   py+1, px+1, py+4, color);
            }
        }
    }

    // ── Text overlay (GDI on the same DC – still works with a DIBSection) ─
    let hdc = data.hdc_back;
    SelectObject(hdc, data.hfont_ui);
    SetBkMode(hdc, OPAQUE);
    SetBkColor(hdc, COLORREF(0x00E8E8E8));
    SetTextColor(hdc, COLORREF(0x00000000));

    let algo_str = match snap.algorithm { Algorithm::Bfs => "BFS", Algorithm::AStar => "A*" };
    let label = format!(" Seed: {:10}  Algo: {} ", snap.seed, algo_str);
    let wide = to_wide(&label);
    TextOutW(hdc, 2, 2, &wide);

    SetBkMode(hdc, TRANSPARENT);
}

// ─── Clipboard ────────────────────────────────────────────────────────────────

unsafe fn copy_to_clipboard(hwnd: HWND, report: &ScoreReport) {
    let secs = report.total_elapsed_secs as u64;
    let time_str = format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60);

    let text = format!(
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
    );

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_len = wide.len() * 2;

    if OpenClipboard(hwnd).is_ok() {
        EmptyClipboard().ok();
        let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len).unwrap();
        let ptr = GlobalLock(hmem) as *mut u16;
        if !ptr.is_null() {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(hmem).ok();
        }
        SetClipboardData(13 /*CF_UNICODETEXT*/, HANDLE(hmem.0 as *mut _)).ok();
        CloseClipboard().ok();
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn get_data<'a>(hwnd: HWND) -> &'a mut WndData {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndData;
    &mut *ptr
}

unsafe fn make_font(height: i32, weight: u32, italic: bool, face: PCWSTR) -> HFONT {
    CreateFontW(
        height, 0, 0, 0,
        weight as i32,
        if italic { 1 } else { 0 },
        0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        (FF_DONTCARE.0 | DEFAULT_PITCH.0) as u32,
        face,
    )
}

unsafe fn set_status_text(hwnd: HWND, part: usize, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    SendMessageW(
        hwnd,
        SB_SETTEXT,
        WPARAM(part),
        LPARAM(wide.as_ptr() as isize),
    );
}

unsafe fn draw_text_w(hdc: HDC, text: &str, rect: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(hdc, &mut wide, rect, flags);
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn mean_u64(v: &[u64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<u64>() as f64 / v.len() as f64
}

fn mean_u32(v: &[u32]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<u32>() as f64 / v.len() as f64
}

fn format_score(n: u64) -> String {
    // Insert thousand separators
    let s = n.to_string();
    let mut result = String::new();
    let offset = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (i % 3 == offset) { result.push(','); }
        result.push(ch);
    }
    result
}

fn tier_color(tier: &str) -> COLORREF {
    match tier {
        "Very Low End" | "Low End"          => COLORREF(0x00B00000),
        "Below Average" | "Average"         => COLORREF(0x00000000),
        "Good" | "Great"                    => COLORREF(0x00008000),
        "Excellent" | "Extreme"             => COLORREF(0x00C00000),
        _                                   => COLORREF(0x00000000),
    }
}

fn mult_sym(m: f64) -> &'static str {
    if      m >= 1.05 { "✓" }
    else if m >= 0.95 { "~" }
    else              { "✗" }
}

fn mult_color(m: f64) -> COLORREF {
    if      m >= 1.05 { COLORREF(0x00008000) }   // green
    else if m >= 0.95 { COLORREF(0x0000A0C0) }   // amber
    else              { COLORREF(0x000000C0) }   // red
}
