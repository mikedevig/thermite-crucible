use std::time::Instant;

use sysinfo::{Networks, System};

use crate::protocol::AliveSample;

pub struct Sampler {
    sys: System,
    nets: Networks,
    last: Instant,
}

impl Sampler {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        let nets = Networks::new_with_refreshed_list();
        Self {
            sys,
            nets,
            last: Instant::now(),
        }
    }

    /// Live network throughput, in bits/sec, since the last call to
    /// `sample()`. Previously this summed `.total_received()` /
    /// `.total_transmitted()`, which are *cumulative* byte counts since the
    /// agent process started (across every interface on the host, not just
    /// VPN traffic) -- so "UP/DOWN" only ever grew and never reflected
    /// actual current speed. `.received()` / `.transmitted()` give the
    /// delta since the last `refresh()`, which combined with the elapsed
    /// wall-clock time gives a real bits/sec rate.
    pub fn sample(&mut self) -> AliveSample {
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();
        self.nets.refresh(true);
        let elapsed = self.last.elapsed().as_secs_f64().max(0.001);
        self.last = Instant::now();

        let mut rx_delta = 0u64;
        let mut tx_delta = 0u64;
        for (_name, data) in self.nets.iter() {
            rx_delta = rx_delta.saturating_add(data.received());
            tx_delta = tx_delta.saturating_add(data.transmitted());
        }

        let ul_bps = (tx_delta as f64 * 8.0 / elapsed).round() as u64;
        let dl_bps = (rx_delta as f64 * 8.0 / elapsed).round() as u64;

        AliveSample {
            ram_used_kib: self.sys.used_memory() / 1024,
            ram_total_kib: self.sys.total_memory() / 1024,
            cpu_pct: self.sys.global_cpu_usage(),
            net_ul_bits: ul_bps,
            net_dl_bits: dl_bps,
        }
    }
}
