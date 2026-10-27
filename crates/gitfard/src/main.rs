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
    handle.current().warn_if_no_client_tokens();

    let cfg = handle.current();
    let cache = match RepoCache::open(&cfg.server.cache_dir, cfg.cache.clone_concurrency) {
        // shortcut: branch_ttl/fetch_timeout are read once; a SIGHUP reload doesn't change them.
        Ok(c) => Arc::new(
            c.with_refresh(cfg.cache.branch_ttl, cfg.cache.fetch_timeout)
                .with_config(handle.clone())
                // shortcut: [upstream] is read once; a SIGHUP reload doesn't change it.
                .with_limits(&handle.current().upstream),
        ),
        Err(e) => {
            eprintln!("gitfard: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listen = handle.current().server.listen;
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%listen, error = %e, "cannot bind API listener");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(%listen, "API listening");
    let app = gitfard::http::router(handle.clone(), cache.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "API server stopped");
        }
    });

    for url in &cfg.warm_list {
        match authorize(&cfg, None, url) {
            Ok(cred) => cache.preheat(url, cred),
            Err(_) => tracing::warn!(url, "warm_list entry outside the allowlist; skipped"),
        }
    }
    // 预热列表 repos stay fresh in the background; others refresh only when requested.
    let (bg, ttl) = (cache.clone(), cfg.cache.branch_ttl);
    let bg_config = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(ttl);
        loop {
            tick.tick().await;
            let cfg = bg_config.current();
            for url in &cfg.warm_list {
                if let Ok(cred) = authorize(&cfg, None, url) {
                    let (cache, url, cred) = (bg.clone(), url.clone(), cred.cloned());
                    tokio::spawn(async move { cache.refresh(&url, cred.as_ref()).await });
                }
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
