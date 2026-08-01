//! What the checksum costs per 4 KiB block, the three ways of computing it.
//!
//! `rustc -O --edition 2024 experiments/crc32c_throughput.rs -o /tmp/crc32c`
//!
//! Reports the hardware path on a CPU that has `crc32`, the table on one that
//! does not — the dispatch is the same call either way.

use std::time::Instant;

#[path = "../crates/molt-fs/src/crc.rs"]
mod crc;

/// Blocks per measurement, enough to run past a few milliseconds.
const ROUNDS: usize = 20_000;

fn main() {
    let block: Vec<u8> = (0..4096).map(|at| at as u8).collect();

    for (name, fold) in [
        ("bitwise", bitwise as fn(&[u8]) -> u32),
        ("molt-fs", |bytes| {
            let mut crc = crc::Crc::new();
            crc.update(bytes);
            crc.finish()
        }),
    ] {
        let start = Instant::now();
        let mut sink = 0;
        for _ in 0..ROUNDS {
            sink ^= fold(&block);
        }
        let elapsed = start.elapsed();
        let mib = (ROUNDS * block.len()) as f64 / (1024.0 * 1024.0);
        println!(
            "{name}: {:.0} MiB/s, {:.0} ns per block ({sink:#010x})",
            mib / elapsed.as_secs_f64(),
            elapsed.as_nanos() as f64 / ROUNDS as f64
        );
    }
}

/// The definition, one bit at a time: what MoltFS computed before the table.
fn bitwise(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}
