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
    /// Manage client access tokens (offline; edits nothing).
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Generate a token; prints it once plus a `[[client_tokens]]` entry to paste into the config.
    New {
        /// Token ID: the unique name logs and revocation refer to.
        #[arg(long)]
        id: String,
        /// Canonical URL prefix this token may read; repeatable. Omit for global.
        #[arg(long)]
        scope: Vec<String>,
        /// RFC 3339 expiry, e.g. 2027-01-01T00:00:00Z. Omit for never.
        #[arg(long)]
        expires: Option<jiff::Timestamp>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Cli::parse().command {
        Command::Serve { config } => config,
        Command::Token(TokenCommand::New { id, scope, expires }) => {
            let new = gitfard::token::new_token(&id, &scope, expires);
            println!("Token (shown only once; store it now):\n\n{}\n", new.token);
            println!("Paste into the config, then send SIGHUP:\n\n{}", new.entry);
            return ExitCode::SUCCESS;
        }
    };
    // Installed first, so a SIGTERM during startup is a clean shutdown rather than a kill.
    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
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
    let git = tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await;
    match git {
        Ok(o) if gitfard::admin::git_version_ok(&String::from_utf8_lossy(&o.stdout)) => {}
        other => {
            let found = other.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
            tracing::error!(?found, "git >= 2.33 is required");
            return ExitCode::FAILURE;
        }
    }
    handle.current().warn_if_no_client_tokens();

    let cfg = handle.current();
    gitfard::exec::grep::init(
        cfg.server.scan_threads,
        cfg.cache.blob_lru,
        cfg.cache.blob_lru_max_object,
    );
    let cache = match RepoCache::open(&cfg.server.cache_dir, cfg.cache.clone_concurrency) {
        Ok(c) => Arc::new(c.with_config(handle.clone())),
        Err(e) => {
            eprintln!("gitfard: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (listen, admin_listen) = (cfg.server.listen, cfg.server.admin_listen);
    let (listener, admin) = match tokio::try_join!(
        tokio::net::TcpListener::bind(listen),
        tokio::net::TcpListener::bind(admin_listen)
    ) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%listen, %admin_listen, error = %e, "cannot bind listeners");
            return ExitCode::FAILURE;
        }
    };
    // Both log the bound address (port 0 picks one).
    tracing::info!(listen = %listener.local_addr().unwrap(), "API listening");
    tracing::info!(admin_listen = %admin.local_addr().unwrap(), "admin listening");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let app = gitfard::http::router(handle.clone(), cache.clone());
    let api = tokio::spawn(async move {
        let mut stopped = stopped;
        let res = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.wait_for(|s| *s).await;
            })
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "API server stopped");
        }
    });
    let admin_app = gitfard::admin::router(handle.clone(), cache.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(admin, admin_app).await {
            tracing::error!(error = %e, "admin server stopped");
        }
    });

    for url in &cfg.warm_list {
        if authorize(&cfg, None, url).is_err() {
            tracing::warn!(url, "warm_list entry not authorized; skipped");
        }
    }
    // 预热列表 repos stay fresh in the background; others refresh only when requested.
    tokio::spawn({
        let (cache, handle) = (cache.clone(), handle.clone());
        async move {
            loop {
                cache.refresh_warm_list();
                let ttl = handle.current().cache.branch_ttl;
                tokio::time::sleep(ttl.max(std::time::Duration::from_secs(1))).await;
            }
        }
    });
    // 淘汰 and 维护.
    tokio::spawn({
        let cache = cache.clone();
        async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                every.tick().await;
                let now = jiff::Zoned::now();
                cache
                    .tick(now.hour() as u16 * 60 + now.minute() as u16)
                    .await;
            }
        }
    });

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
    // Stop accepting; in-flight requests get at most one request deadline. Returning drops the
    // runtime's tasks, which aborts clones and kills git children (kill_on_drop); clone residue
    // in tmp/ is wiped at the next start.
    tracing::info!("shutting down");
    stop.send_replace(true);
    let grace = handle.current().server.request_deadline;
    if tokio::time::timeout(grace, api).await.is_err() {
        tracing::warn!(?grace, "in-flight requests cut off at shutdown");
    }
    ExitCode::SUCCESS
}
