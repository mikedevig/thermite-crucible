use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::time::{Duration, Instant};

use crate::maze::{Cell, Maze, MAZE_SIZE};
use crate::solver::{Algorithm, bfs, astar};

pub const BFS_TARGET:   u64   = 400_000;
pub const ASTAR_TARGET: u64   = 1_200_000;

/// Snapshot of one maze after solving, for UI display.
#[derive(Clone)]
pub struct MazeSnapshot {
    pub grid:      Box<[[Cell; MAZE_SIZE]; MAZE_SIZE]>,
    /// All cells visited during the last solve (heatmap)
    pub explored:  Vec<(u8, u8)>,
    /// Final solution path
    pub path:      Vec<(u8, u8)>,
    pub algorithm: Algorithm,
    pub seed:      u64,
    pub thread_id: usize,
}

/// All shared mutable state between worker threads and the UI thread.
pub struct SharedState {
    // Atomic counters
    pub bfs_completed:   AtomicU64,
    pub astar_completed: AtomicU64,
    pub wrong_solutions: AtomicU64,
    pub done:            AtomicBool,

    // Per-thread solve counts (length = num_threads)
    pub per_thread_completed: Vec<AtomicU64>,

    // Rolling stat windows (try_lock only from UI)
    pub bfs_times_ns:       Mutex<Vec<u64>>,   // cap 50_000
    pub astar_times_ns:     Mutex<Vec<u64>>,   // cap 50_000
    pub bfs_explored:       Mutex<Vec<u32>>,   // cap 20_000
    pub astar_explored:     Mutex<Vec<u32>>,   // cap 20_000
    pub bfs_path_lengths:   Mutex<Vec<u32>>,   // cap 20_000
    pub astar_path_lengths: Mutex<Vec<u32>>,   // cap 20_000
    pub gen_times_ns:       Mutex<Vec<u64>>,   // cap 10_000

    // Throughput samples: (elapsed_secs, combined_solves_per_sec)
    pub throughput_samples: Mutex<Vec<(f64, f64)>>,

    // Latest maze snapshot for UI
    pub latest_snapshot: Mutex<Option<MazeSnapshot>>,

    // Benchmark start time
    pub start_time: Mutex<Option<Instant>>,
}

impl SharedState {
    pub fn new(num_threads: usize) -> Arc<Self> {
        Arc::new(Self {
            bfs_completed:        AtomicU64::new(0),
            astar_completed:      AtomicU64::new(0),
            wrong_solutions:      AtomicU64::new(0),
            done:                 AtomicBool::new(false),
            per_thread_completed: (0..num_threads).map(|_| AtomicU64::new(0)).collect(),
            bfs_times_ns:         Mutex::new(Vec::with_capacity(50_000)),
            astar_times_ns:       Mutex::new(Vec::with_capacity(50_000)),
            bfs_explored:         Mutex::new(Vec::with_capacity(20_000)),
            astar_explored:       Mutex::new(Vec::with_capacity(20_000)),
            bfs_path_lengths:     Mutex::new(Vec::with_capacity(20_000)),
            astar_path_lengths:   Mutex::new(Vec::with_capacity(20_000)),
            gen_times_ns:         Mutex::new(Vec::with_capacity(10_000)),
            throughput_samples:   Mutex::new(Vec::new()),
            latest_snapshot:      Mutex::new(None),
            start_time:           Mutex::new(None),
        })
    }
}

// ─── Ring-buffer push helpers ─────────────────────────────────────────────────
//
// These replace the original O(n) `remove(0)` with an amortised drain:
// when a batch fills the ring, we evict 25 % of the oldest entries in one
// contiguous memcpy instead of removing elements one-by-one.

#[inline]
fn flush_batch_u64(v: &mut Vec<u64>, batch: &[u64], cap: usize) {
    let total = v.len() + batch.len();
    if total > cap {
        let excess = total - cap;
        // Drain at least 25 % of cap to amortise the shift cost.
        let to_remove = excess.max(cap / 4).min(v.len());
        v.drain(0..to_remove);
    }
    let space = cap.saturating_sub(v.len());
    let take  = batch.len().min(space);
    v.extend_from_slice(&batch[batch.len() - take..]);
}

#[inline]
fn flush_batch_u32(v: &mut Vec<u32>, batch: &[u32], cap: usize) {
    let total = v.len() + batch.len();
    if total > cap {
        let excess = total - cap;
        let to_remove = excess.max(cap / 4).min(v.len());
        v.drain(0..to_remove);
    }
    let space = cap.saturating_sub(v.len());
    let take  = batch.len().min(space);
    v.extend_from_slice(&batch[batch.len() - take..]);
}

// ─── Worker thread ────────────────────────────────────────────────────────────

// How many solve iterations to accumulate locally before paying the cost of
// locking the shared stat vecs. Higher = less contention, slightly coarser stats.
const FLUSH_EVERY: usize = 32;

fn worker_loop(thread_id: usize, seeds: Arc<Vec<u64>>, shared: Arc<SharedState>) {
    let mut seed_idx = thread_id % seeds.len();
    let mut last_snapshot = Instant::now();

    // ── Per-thread local accumulation buffers ─────────────────────────────
    let mut lb_times:  Vec<u64> = Vec::with_capacity(FLUSH_EVERY);
    let mut la_times:  Vec<u64> = Vec::with_capacity(FLUSH_EVERY);
    let mut lb_exp:    Vec<u32> = Vec::with_capacity(FLUSH_EVERY);
    let mut la_exp:    Vec<u32> = Vec::with_capacity(FLUSH_EVERY);
    let mut lb_paths:  Vec<u32> = Vec::with_capacity(FLUSH_EVERY);
    let mut la_paths:  Vec<u32> = Vec::with_capacity(FLUSH_EVERY);
    let mut lg_times:  Vec<u64> = Vec::with_capacity(FLUSH_EVERY);

    let mut iter: usize = 0;

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 1 – BFS only
    // Each worker runs BFS independently until the global BFS_TARGET is met.
    // ═══════════════════════════════════════════════════════════════════════
    loop {
        if shared.done.load(Acquire) { return; }
        if shared.bfs_completed.load(Relaxed) >= BFS_TARGET { break; }

        let seed = seeds[seed_idx];
        seed_idx = (seed_idx + 1) % seeds.len();

        let snap_due = last_snapshot.elapsed().as_millis() >= 50;
        let snap_guard = if snap_due {
            shared.latest_snapshot.try_lock().ok()
        } else {
            None
        };
        let collect_cells = snap_guard.is_some();

        let gen_start = Instant::now();
        let maze = Maze::new(seed);
        lg_times.push(gen_start.elapsed().as_nanos() as u64);

        let bfs_result = bfs::solve(&maze, collect_cells);
        lb_times.push(bfs_result.time_ns);
        lb_exp.push(bfs_result.cells_explored);
        lb_paths.push(bfs_result.path_length);

        shared.bfs_completed.fetch_add(1, Relaxed);
        shared.per_thread_completed[thread_id].fetch_add(1, Relaxed);

        if let Some(mut guard) = snap_guard {
            last_snapshot = Instant::now();
            *guard = Some(MazeSnapshot {
                grid:      maze.grid.clone(),
                explored:  bfs_result.explored_cells,
                path:      bfs_result.path,
                algorithm: Algorithm::Bfs,
                seed,
                thread_id,
            });
        }
        if snap_due && !collect_cells {
            last_snapshot = Instant::now();
        }

        iter += 1;
        if iter % FLUSH_EVERY == 0 {
            if let Ok(mut v) = shared.bfs_times_ns.try_lock() {
                flush_batch_u64(&mut v, &lb_times, 50_000);
            }
            if let Ok(mut v) = shared.bfs_explored.try_lock() {
                flush_batch_u32(&mut v, &lb_exp, 20_000);
            }
            if let Ok(mut v) = shared.bfs_path_lengths.try_lock() {
                flush_batch_u32(&mut v, &lb_paths, 20_000);
            }
            if let Ok(mut v) = shared.gen_times_ns.try_lock() {
                flush_batch_u64(&mut v, &lg_times, 10_000);
            }
            lb_times.clear(); lb_exp.clear(); lb_paths.clear(); lg_times.clear();
        }
    }

    // Flush remaining BFS locals before switching phases.
    if let Ok(mut v) = shared.bfs_times_ns.try_lock()  { flush_batch_u64(&mut v, &lb_times, 50_000); }
    if let Ok(mut v) = shared.bfs_explored.try_lock()  { flush_batch_u32(&mut v, &lb_exp,   20_000); }
    if let Ok(mut v) = shared.bfs_path_lengths.try_lock() { flush_batch_u32(&mut v, &lb_paths, 20_000); }
    if let Ok(mut v) = shared.gen_times_ns.try_lock()  { flush_batch_u64(&mut v, &lg_times, 10_000); }
    lb_times.clear(); lb_exp.clear(); lb_paths.clear(); lg_times.clear();
    iter = 0;

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 2 – A* only
    // All workers pivot to A* once BFS is done and run until ASTAR_TARGET.
    // ═══════════════════════════════════════════════════════════════════════
    loop {
        if shared.done.load(Acquire) { return; }
        if shared.astar_completed.load(Relaxed) >= ASTAR_TARGET { break; }

        let seed = seeds[seed_idx];
        seed_idx = (seed_idx + 1) % seeds.len();

        let snap_due = last_snapshot.elapsed().as_millis() >= 50;
        let snap_guard = if snap_due {
            shared.latest_snapshot.try_lock().ok()
        } else {
            None
        };
        let collect_cells = snap_guard.is_some();

        let gen_start = Instant::now();
        let maze = Maze::new(seed);
        lg_times.push(gen_start.elapsed().as_nanos() as u64);

        let astar_result = astar::solve(&maze, collect_cells);
        la_times.push(astar_result.time_ns);
        la_exp.push(astar_result.cells_explored);
        la_paths.push(astar_result.path_length);

        shared.astar_completed.fetch_add(1, Relaxed);
        shared.per_thread_completed[thread_id].fetch_add(1, Relaxed);

        if let Some(mut guard) = snap_guard {
            last_snapshot = Instant::now();
            *guard = Some(MazeSnapshot {
                grid:      maze.grid.clone(),
                explored:  astar_result.explored_cells,
                path:      astar_result.path,
                algorithm: Algorithm::AStar,
                seed,
                thread_id,
            });
        }
        if snap_due && !collect_cells {
            last_snapshot = Instant::now();
        }

        iter += 1;
        if iter % FLUSH_EVERY == 0 {
            if let Ok(mut v) = shared.astar_times_ns.try_lock() {
                flush_batch_u64(&mut v, &la_times, 50_000);
            }
            if let Ok(mut v) = shared.astar_explored.try_lock() {
                flush_batch_u32(&mut v, &la_exp, 20_000);
            }
            if let Ok(mut v) = shared.astar_path_lengths.try_lock() {
                flush_batch_u32(&mut v, &la_paths, 20_000);
            }
            if let Ok(mut v) = shared.gen_times_ns.try_lock() {
                flush_batch_u64(&mut v, &lg_times, 10_000);
            }
            la_times.clear(); la_exp.clear(); la_paths.clear(); lg_times.clear();
        }
    }

    // Flush remaining A* locals then signal done (first thread to finish wins).
    if let Ok(mut v) = shared.astar_times_ns.try_lock()     { flush_batch_u64(&mut v, &la_times, 50_000); }
    if let Ok(mut v) = shared.astar_explored.try_lock()     { flush_batch_u32(&mut v, &la_exp,   20_000); }
    if let Ok(mut v) = shared.astar_path_lengths.try_lock() { flush_batch_u32(&mut v, &la_paths, 20_000); }
    if let Ok(mut v) = shared.gen_times_ns.try_lock()       { flush_batch_u64(&mut v, &lg_times, 10_000); }

    shared.done.store(true, Release);
}

// ─── Monitor thread (throughput sampling, 1/sec) ─────────────────────────────

fn monitor_loop(shared: Arc<SharedState>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if shared.done.load(Acquire) { break; }

        let elapsed = {
            let st = shared.start_time.lock().unwrap();
            st.as_ref().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
        };

        if elapsed > 0.0 {
            let total = shared.bfs_completed.load(Relaxed)
                      + shared.astar_completed.load(Relaxed);
            let per_sec = total as f64 / elapsed;
            if let Ok(mut s) = shared.throughput_samples.try_lock() {
                s.push((elapsed, per_sec));
            }
        }
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn start(shared: Arc<SharedState>, num_threads: usize, seeds: Arc<Vec<u64>>) {

    // Record start time
    *shared.start_time.lock().unwrap() = Some(Instant::now());

    // Spawn workers
    for i in 0..num_threads {
        let s = Arc::clone(&shared);
        let seeds_clone = Arc::clone(&seeds);
        std::thread::spawn(move || worker_loop(i, seeds_clone, s));
    }

    // Spawn monitor
    let s = Arc::clone(&shared);
    std::thread::spawn(move || monitor_loop(s));
}
