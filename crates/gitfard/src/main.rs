//! gitfard: the gitfar server.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gitfard::authz::authorize;
use gitfard::cache::RepoCache;
use gitfard::config::{ConfigHandle, LogFormat};
use tokio::signal::unix::{signal, SignalKind};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let Command::Serve { config } = Cli::parse().command;
    let handle = match ConfigHandle::load(config) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("gitfard: {e}");
            return ExitCode::FAILURE;
        }
    };
    let logs = tracing_subscriber::fmt();
    match handle.current().server.log_format {
        LogFormat::Json => logs.json().init(),
        LogFormat::Pretty => logs.pretty().init(),
    }
    tracing::info!("config loaded");

    let cfg = handle.current();
    let cache = match RepoCache::open(&cfg.server.cache_dir, cfg.cache.clone_concurrency) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("gitfard: {e}");
            return ExitCode::FAILURE;
        }
    };
    for url in &cfg.warm_list {
        match authorize(&cfg, None, url) {
            Ok(cred) => cache.preheat(url, cred),
            Err(_) => tracing::warn!(url, "warm_list entry outside the allowlist; skipped"),
        }
    }

    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    loop {
        tokio::select! {
            _ = hup.recv() => {
                // reload() logs its own outcome; failure keeps the old config.
                let _ = handle.reload();
            }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    ExitCode::SUCCESS
}
