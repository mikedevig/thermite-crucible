use std::sync::atomic::Ordering::Relaxed;
use crate::benchmark::{SharedState, BFS_TARGET, ASTAR_TARGET};

#[derive(Debug, Clone)]
pub struct ScoreReport {
    // Inputs
    pub bfs_solves_per_sec:      f64,
    pub astar_solves_per_sec:    f64,
    pub combined_solves_per_sec: f64,
    pub total_elapsed_secs:      f64,
    pub num_threads:             usize,

    // The 9 score components
    pub speed_base:                        f64,
    pub path_optimality_multiplier:        f64,
    pub exploration_efficiency_multiplier: f64,
    pub consistency_multiplier:            f64,
    pub thermal_sustain_multiplier:        f64,
    pub parallelism_efficiency_multiplier: f64,
    pub peak_burst_multiplier:             f64,
    pub algorithm_duel_multiplier:         f64,
    pub correctness_multiplier:            f64,

    // Final result
    pub final_score: u64,
    pub tier: &'static str,

    // Diagnostics
    pub avg_bfs_time_us:      f64,
    pub avg_astar_time_us:    f64,
    pub avg_bfs_explored:     f64,
    pub avg_astar_explored:   f64,
    pub avg_bfs_path:         f64,
    pub avg_astar_path:       f64,
    pub wrong_count:          u64,
    pub avg_gen_time_us:      f64,
    pub peak_solves_per_sec:  f64,
    pub worst_solves_per_sec: f64,
}

// ─── Statistical helpers ──────────────────────────────────────────────────────

fn mean_u64(v: &[u64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<u64>() as f64 / v.len() as f64
}

fn mean_u32(v: &[u32]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<u32>() as f64 / v.len() as f64
}

fn std_dev_u64(v: &[u64]) -> f64 {
    if v.len() < 2 { return 0.0; }
    let m = mean_u64(v);
    let var = v.iter().map(|&x| { let d = x as f64 - m; d * d }).sum::<f64>() / v.len() as f64;
    var.sqrt()
}

fn mean_f64(v: &[f64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<f64>() / v.len() as f64
}

pub fn tier_for(score: u64) -> &'static str {
    match score {
        0..=99_999              => "Very Low End",
        100_000..=499_999       => "Low End",
        500_000..=999_999       => "Below Average",
        1_000_000..=4_999_999   => "Average",
        5_000_000..=9_999_999   => "Good",
        10_000_000..=24_999_999 => "Great",
        25_000_000..=49_999_999 => "Excellent",
        _                       => "Extreme",
    }
}

// ─── Main scoring function ────────────────────────────────────────────────────

pub fn compute(shared: &SharedState, num_threads: usize) -> ScoreReport {
    // Lock all data (benchmark is done, no contention)
    let bfs_times     = shared.bfs_times_ns.lock().unwrap();
    let astar_times   = shared.astar_times_ns.lock().unwrap();
    let bfs_exp       = shared.bfs_explored.lock().unwrap();
    let astar_exp     = shared.astar_explored.lock().unwrap();
    let bfs_paths     = shared.bfs_path_lengths.lock().unwrap();
    let astar_paths   = shared.astar_path_lengths.lock().unwrap();
    let gen_times     = shared.gen_times_ns.lock().unwrap();
    let throughput    = shared.throughput_samples.lock().unwrap();
    let start_opt     = shared.start_time.lock().unwrap();

    let elapsed = start_opt.as_ref()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(1.0)
        .max(0.001);

    // ── Component 1: Speed Base ───────────────────────────────────────────────
    let combined_per_sec = (BFS_TARGET + ASTAR_TARGET) as f64 / elapsed;
    let speed_base = combined_per_sec * 1_000.0;

    // ── Component 2: Path Optimality (0.50 – 1.20) ───────────────────────────
    let wrong = shared.wrong_solutions.load(Relaxed);
    let total_pairs = BFS_TARGET as f64;
    let optimality_rate = (1.0 - wrong as f64 / total_pairs).clamp(0.0, 1.0);
    let path_optimality_multiplier = 0.50 + optimality_rate * 0.70;

    // ── Component 3: Exploration Efficiency (0.70 – 1.30) ────────────────────
    let avg_bfs_explored   = mean_u32(&bfs_exp);
    let avg_astar_explored = mean_u32(&astar_exp);
    let ratio = if avg_bfs_explored == 0.0 {
        1.0
    } else {
        (avg_astar_explored / avg_bfs_explored).clamp(0.0, 1.0)
    };
    let exploration_efficiency_multiplier = 0.70 + (1.0 - ratio) * 0.60;

    // ── Component 4: Consistency (0.70 – 1.20) ───────────────────────────────
    let m  = mean_u64(&astar_times);
    let sd = std_dev_u64(&astar_times);
    let cv = if m == 0.0 { 0.0 } else { sd / m };
    let consistency_multiplier = (1.20 - cv * 1.50).clamp(0.70, 1.20);

    // ── Component 5: Thermal Sustain (0.60 – 1.10) ───────────────────────────
    let thermal_sustain_multiplier = if throughput.len() < 8 {
        1.0
    } else {
        let q_size = throughput.len() / 4;
        let q1_vals: Vec<f64> = throughput[..q_size].iter().map(|s| s.1).collect();
        let q4_vals: Vec<f64> = throughput[3 * q_size..].iter().map(|s| s.1).collect();
        let q1 = mean_f64(&q1_vals);
        let q4 = mean_f64(&q4_vals);
        let ratio = if q1 == 0.0 { 1.0 } else { q4 / q1 };
        if      ratio >= 0.95 { 1.10 }
        else if ratio >= 0.85 { 1.00 }
        else if ratio >= 0.70 { 0.85 }
        else if ratio >= 0.55 { 0.72 }
        else                  { 0.60 }
    };

    // ── Component 6: Parallelism Efficiency (0.70 – 1.15) ───────────────────
    let counts: Vec<u64> = shared.per_thread_completed.iter()
        .map(|a| a.load(Relaxed))
        .collect();
    let min_c = counts.iter().copied().min().unwrap_or(0);
    let max_c = counts.iter().copied().max().unwrap_or(1);
    let balance = if max_c == 0 { 1.0 } else { (min_c as f64 / max_c as f64).clamp(0.0, 1.0) };
    let parallelism_efficiency_multiplier = (0.70 + balance * 0.45).clamp(0.70, 1.15);

    // ── Component 7: Peak Burst (1.00 – 1.20) ────────────────────────────────
    let tp_vals: Vec<f64> = throughput.iter().map(|s| s.1).collect();
    let overall_avg = mean_f64(&tp_vals);
    let mut best_5s: f64 = 0.0;
    for i in 0..throughput.len() {
        let window_end = throughput[i].0 + 5.0;
        let window_vals: Vec<f64> = throughput[i..]
            .iter()
            .take_while(|s| s.0 <= window_end)
            .map(|s| s.1)
            .collect();
        let wa = mean_f64(&window_vals);
        if wa > best_5s { best_5s = wa; }
    }
    let burst_ratio = if overall_avg == 0.0 { 1.0 } else { best_5s / overall_avg };
    let peak_burst_multiplier = (1.0 + (burst_ratio - 1.0) * 0.40).clamp(1.00, 1.20);

    // ── Component 8: Algorithm Duel (0.80 – 1.20) ────────────────────────────
    let avg_bfs_ns   = mean_u64(&bfs_times);
    let avg_astar_ns = mean_u64(&astar_times);
    let duel_ratio = if avg_astar_ns == 0.0 { 1.0 } else { avg_bfs_ns / avg_astar_ns };
    let algorithm_duel_multiplier = if duel_ratio >= 3.0 {
        1.20
    } else if duel_ratio >= 2.0 {
        1.10 + (duel_ratio - 2.0) * 0.10
    } else if duel_ratio >= 1.0 {
        0.90 + (duel_ratio - 1.0) * 0.20
    } else {
        (0.80 + duel_ratio * 0.10).clamp(0.80, 0.90)
    };

    // ── Component 9: Correctness (0.50 – 1.05) ───────────────────────────────
    let correctness_multiplier = if wrong == 0 {
        1.05
    } else {
        let error_rate = wrong as f64 / BFS_TARGET as f64;
        (1.0 - error_rate * 100.0).clamp(0.50, 1.00)
    };

    // ── Final score ───────────────────────────────────────────────────────────
    let raw = speed_base
        * path_optimality_multiplier
        * exploration_efficiency_multiplier
        * consistency_multiplier
        * thermal_sustain_multiplier
        * parallelism_efficiency_multiplier
        * peak_burst_multiplier
        * algorithm_duel_multiplier
        * correctness_multiplier;

    let final_score = raw.round() as u64;
    let tier = tier_for(final_score);

    // ── Diagnostics ───────────────────────────────────────────────────────────
    let bfs_solves_per_sec   = BFS_TARGET as f64   / elapsed;
    let astar_solves_per_sec = ASTAR_TARGET as f64 / elapsed;
    let peak_solves_per_sec  = tp_vals.iter().cloned().fold(0.0f64, f64::max);
    let worst_solves_per_sec = tp_vals.iter().cloned().fold(f64::MAX, f64::min);

    ScoreReport {
        bfs_solves_per_sec,
        astar_solves_per_sec,
        combined_solves_per_sec: combined_per_sec,
        total_elapsed_secs: elapsed,
        num_threads,

        speed_base,
        path_optimality_multiplier,
        exploration_efficiency_multiplier,
        consistency_multiplier,
        thermal_sustain_multiplier,
        parallelism_efficiency_multiplier,
        peak_burst_multiplier,
        algorithm_duel_multiplier,
        correctness_multiplier,

        final_score,
        tier,

        avg_bfs_time_us:    avg_bfs_ns   / 1_000.0,
        avg_astar_time_us:  avg_astar_ns / 1_000.0,
        avg_bfs_explored,
        avg_astar_explored,
        avg_bfs_path:    mean_u32(&bfs_paths),
        avg_astar_path:  mean_u32(&astar_paths),
        wrong_count:     wrong,
        avg_gen_time_us: mean_u64(&gen_times) / 1_000.0,
        peak_solves_per_sec:  if peak_solves_per_sec  == 0.0   { 0.0 } else { peak_solves_per_sec },
        worst_solves_per_sec: if worst_solves_per_sec == f64::MAX { 0.0 } else { worst_solves_per_sec },
    }
}
