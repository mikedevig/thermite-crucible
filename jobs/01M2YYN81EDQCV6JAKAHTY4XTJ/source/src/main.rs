#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![allow(dead_code)] // Some code is platform-only; used by ui/windows.rs or ui/linux.rs

mod maze;
mod solver;
mod benchmark;
mod scoring;
mod ui;

fn main() {
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let shared = benchmark::SharedState::new(num_threads);

    #[cfg(target_os = "windows")]
    {
        ui::windows::run(std::sync::Arc::clone(&shared), num_threads)
            .expect("UI failed to run");
    }

    #[cfg(not(target_os = "windows"))]
    {
        ui::linux::run(std::sync::Arc::clone(&shared), num_threads)
            .expect("UI failed to run");
    }
}
