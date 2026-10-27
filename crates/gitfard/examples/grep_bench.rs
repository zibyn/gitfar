//! Manual benchmark (#28): full-tree `grep` on a local repo, e.g. a bare kernel clone.
//!
//!     cargo run --release -p gitfard --example grep_bench -- <repo.git> [rev] [threads]
//!
//! Prints wall/CPU seconds per query; each runs twice (cold, then warm blob LRU).
//! Compare with `LC_ALL=C git grep -c -I -E -e <pattern> <rev>` (`/usr/bin/time`).
//!
//! Recorded 2026-10-27, Linux kernel HEAD (97k blobs, 1.67 GB), 16 threads, warm page cache:
//!
//! | query                          | first run (cold LRU)  | repeat (warm LRU)     |
//! |--------------------------------|-----------------------|-----------------------|
//! | literal `EXPORT_SYMBOL_GPL`    | 0.76 s wall, 6.00 CPU s | 0.22 s wall, 1.05 CPU s |
//! | regex `\w+_lock\(`             | 0.45 s wall, 1.51 CPU s | 0.46 s wall, 1.50 CPU s |
//! | regex `[A-Z]{3,}_[0-9]+_[A-Z]+` | 0.60 s wall, 3.64 CPU s | 0.60 s wall, 3.57 CPU s |
//!
//! The literal's line count (21267) equals bench/kernel's; its cold run reads every blob
//! (bench: 6.6 CPU s, 0.76 s wall at 16 threads), and later queries hit the LRU. Bench's
//! reference `git grep` literal: ~1.5 s wall. A cold *disk* cache made the first run 10.6 s.

use std::time::Instant;

use gitfar_proto::{GrepOutputMode, GrepParams};

/// User+system CPU seconds of this process (Linux; 100 Hz ticks).
fn cpu_s() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo = args
        .first()
        .expect("usage: grep_bench <repo.git> [rev] [threads]");
    let rev = args.get(1).map_or("HEAD", String::as_str);
    let threads = args.get(2).map_or(0, |t| t.parse().unwrap());
    gitfard::exec::grep::init(threads, None, 8_000_000);
    let queries = [
        ("literal", "EXPORT_SYMBOL_GPL", true),
        ("regex", r"\w+_lock\(", false),
        ("regex", "[A-Z]{3,}_[0-9]+_[A-Z]+", false),
    ];
    for (kind, pattern, fixed) in queries {
        for run in ["cold", "warm"] {
            let params = GrepParams {
                repo: String::new(),
                rev: None,
                pattern: pattern.into(),
                path: vec![],
                include: vec![],
                exclude: vec![],
                fixed_string: Some(fixed),
                ignore_case: None,
                before: None,
                after: None,
                context: None,
                output_mode: Some(GrepOutputMode::Count),
                max_results: Some(u32::MAX),
                skip_large: None,
            };
            let (wall, cpu) = (Instant::now(), cpu_s());
            let (text, pages) = gitfard::exec::grep::grep_all(repo.as_ref(), rev, params)
                .unwrap_or_else(|e| panic!("{e:?}"));
            let lines: u64 = text
                .lines()
                .map(|l| l.rsplit_once(':').unwrap().1.parse::<u64>().unwrap())
                .sum();
            println!(
                "{kind} {pattern:?} {run}: wall {:.2}s cpu {:.2}s  files {} lines {lines} pages {pages}",
                wall.elapsed().as_secs_f64(),
                cpu_s() - cpu,
                text.lines().count(),
            );
        }
    }
}
