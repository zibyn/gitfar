//! gitfar: the CLI client (spec §5). Talks only to the HTTP API (§3.1).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::parser::ValueSource;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use gitfar_proto::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "gitfar",
    version,
    about = "Search remote git repositories through a gitfard server"
)]
struct Cli {
    /// Server URL; overrides GITFAR_URL and the config file.
    #[arg(long, global = true)]
    url: Option<String>,
    /// Print the full response (or error) object as JSON on stdout; no footer.
    #[arg(long, global = true)]
    json: bool,
    /// Seconds to keep retrying while the repo is warming; 0 disables retries.
    #[arg(long, global = true, value_name = "SECS", default_value_t = 90)]
    wait: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Global flags; everything else on a subcommand conflicts with `--cursor`.
const GLOBALS: [&str; 3] = ["url", "json", "wait"];

#[derive(Subcommand)]
enum Cmd {
    /// Read a file with line numbers.
    Read(ReadArgs),
    /// Regex search over the tree.
    Grep(GrepArgs),
    /// List paths (ls/find).
    List(ListArgs),
    /// Commit log, one line per commit.
    Log(LogArgs),
    /// Show a commit or diff a range.
    Show(ShowArgs),
    /// Line-range blame.
    Blame(BlameArgs),
    /// Show the token id and scope the server sees.
    Whoami,
}

#[derive(Args)]
struct ReadArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    #[arg(required_unless_present = "cursor")]
    path: Option<String>,
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    start_line: Option<u32>,
    #[arg(long)]
    end_line: Option<u32>,
    /// Continue from a previous response's `next`; no other arguments allowed.
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Args)]
struct GrepArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    #[arg(required_unless_present = "cursor")]
    pattern: Option<String>,
    path: Vec<String>,
    #[arg(long)]
    rev: Option<String>,
    #[arg(short = 'i', long)]
    ignore_case: bool,
    #[arg(short = 'F', long)]
    fixed_string: bool,
    /// Print only matching file paths.
    #[arg(short = 'l', long = "files-with-matches", conflicts_with = "count")]
    files: bool,
    /// Print `path:N` match counts.
    #[arg(short = 'c', long)]
    count: bool,
    #[arg(short = 'A', long)]
    after: Option<u32>,
    #[arg(short = 'B', long)]
    before: Option<u32>,
    #[arg(short = 'C', long)]
    context: Option<u32>,
    #[arg(short = 'm', long)]
    max_results: Option<u32>,
    #[arg(long)]
    include: Vec<String>,
    #[arg(long)]
    exclude: Vec<String>,
    /// Also search blobs over 10 MB.
    #[arg(long)]
    no_skip_large: bool,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    path: Option<String>,
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    glob: Option<String>,
    #[arg(long)]
    max_depth: Option<u32>,
    #[arg(long, value_parser = ["file", "dir", "any"])]
    r#type: Option<String>,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Args)]
struct LogArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    path: Vec<String>,
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    max_count: Option<u32>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    author: Option<String>,
    #[arg(long)]
    grep: Option<String>,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Args)]
struct ShowArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    #[arg(required_unless_present = "cursor")]
    rev: Option<String>,
    path: Vec<String>,
    #[arg(long, value_parser = ["stat", "name-only", "patch"])]
    format: Option<String>,
    #[arg(long)]
    context_lines: Option<u32>,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Args)]
struct BlameArgs {
    #[arg(required_unless_present = "cursor")]
    repo: Option<String>,
    #[arg(required_unless_present = "cursor")]
    path: Option<String>,
    #[arg(long, required_unless_present = "cursor")]
    start_line: Option<u32>,
    #[arg(long, required_unless_present = "cursor")]
    end_line: Option<u32>,
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    cursor: Option<String>,
}

/// A clap-validated enum value into its gitfar-proto type.
fn enum_arg<T: DeserializeOwned>(s: Option<String>) -> Option<T> {
    s.map(|s| serde_json::from_value(json!(s)).expect("clap restricted the values"))
}

fn flag(b: bool) -> Option<bool> {
    b.then_some(true)
}

fn to_json(p: impl serde::Serialize) -> Value {
    serde_json::to_value(p).expect("params serialize")
}

/// The request body: `{cursor}` alone, or the primitive's params. Positionals are
/// guaranteed by clap when there is no cursor.
fn request(cmd: Cmd) -> (Primitive, Value) {
    macro_rules! body {
        ($a:ident, $p:expr, $params:expr) => {
            match $a.cursor {
                Some(c) => ($p, json!({ "cursor": c })),
                None => ($p, to_json($params)),
            }
        };
    }
    match cmd {
        Cmd::Read(a) => body!(
            a,
            Primitive::Read,
            ReadParams {
                repo: a.repo.unwrap(),
                rev: a.rev,
                path: a.path.unwrap(),
                start_line: a.start_line,
                end_line: a.end_line,
            }
        ),
        Cmd::Grep(a) => body!(
            a,
            Primitive::Grep,
            GrepParams {
                repo: a.repo.unwrap(),
                rev: a.rev,
                pattern: a.pattern.unwrap(),
                path: a.path,
                include: a.include,
                exclude: a.exclude,
                fixed_string: flag(a.fixed_string),
                ignore_case: flag(a.ignore_case),
                before: a.before,
                after: a.after,
                context: a.context,
                output_mode: if a.files {
                    Some(GrepOutputMode::Files)
                } else if a.count {
                    Some(GrepOutputMode::Count)
                } else {
                    None
                },
                max_results: a.max_results,
                skip_large: a.no_skip_large.then_some(false),
            }
        ),
        Cmd::List(a) => body!(
            a,
            Primitive::List,
            ListParams {
                repo: a.repo.unwrap(),
                rev: a.rev,
                path: a.path,
                glob: a.glob,
                max_depth: a.max_depth,
                r#type: enum_arg(a.r#type),
            }
        ),
        Cmd::Log(a) => body!(
            a,
            Primitive::Log,
            LogParams {
                repo: a.repo.unwrap(),
                rev: a.rev,
                path: a.path,
                max_count: a.max_count,
                since: a.since,
                until: a.until,
                author: a.author,
                grep: a.grep,
            }
        ),
        Cmd::Show(a) => body!(
            a,
            Primitive::Show,
            ShowParams {
                repo: a.repo.unwrap(),
                rev: a.rev.unwrap(),
                path: a.path,
                format: enum_arg(a.format),
                context_lines: a.context_lines,
            }
        ),
        Cmd::Blame(a) => body!(
            a,
            Primitive::Blame,
            BlameParams {
                repo: a.repo.unwrap(),
                rev: a.rev,
                path: a.path.unwrap(),
                start_line: a.start_line.unwrap(),
                end_line: a.end_line.unwrap(),
            }
        ),
        Cmd::Whoami => unreachable!("handled before"),
    }
}

/// Parse argv; `--cursor` with any other subcommand argument is a usage error (exit 2).
fn parse() -> Cli {
    let mut cmd = Cli::command();
    let m = cmd.get_matches_mut();
    if let Some((name, sub)) = m.subcommand() {
        let ids: Vec<String> = cmd
            .find_subcommand(name)
            .expect("parsed")
            .get_arguments()
            .map(|a| a.get_id().to_string())
            .filter(|id| !GLOBALS.contains(&id.as_str()))
            .collect();
        let given = |id: &str| sub.value_source(id) == Some(ValueSource::CommandLine);
        if ids.iter().any(|id| id == "cursor") && given("cursor") {
            if let Some(other) = ids.iter().find(|id| *id != "cursor" && given(id)) {
                cmd.error(
                    clap::error::ErrorKind::ArgumentConflict,
                    format!("--cursor cannot be combined with other arguments (got {other})"),
                )
                .exit();
            }
        }
    }
    Cli::from_arg_matches(&m).unwrap_or_else(|e| e.exit())
}

#[derive(Deserialize, Default)]
struct FileConfig {
    url: Option<String>,
    token: Option<String>,
}

/// `$XDG_CONFIG_HOME/gitfar/config.toml` (falling back to `~/.config`); missing file = empty.
fn file_config() -> Result<FileConfig, String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")));
    let Some(path) = dir.map(|d| d.join("gitfar/config.toml")) else {
        return Ok(FileConfig::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// A failure to report as `gitfar: <code>: <message>` with an exit code.
struct Fail {
    code: String,
    message: String,
    exit: u8,
}

impl Fail {
    fn new(code: &str, message: impl ToString, exit: u8) -> Fail {
        Fail {
            code: code.into(),
            message: message.to_string(),
            exit,
        }
    }
}

fn main() -> ExitCode {
    let cli = parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            eprintln!("gitfar: {}: {}", f.code, f.message);
            ExitCode::from(f.exit)
        }
    }
}

fn run(cli: Cli) -> Result<u8, Fail> {
    let file = file_config().map_err(|e| Fail::new("usage", e, 2))?;
    let url = cli
        .url
        .or_else(|| env("GITFAR_URL"))
        .or(file.url)
        .ok_or_else(|| {
            Fail::new(
                "usage",
                "no server URL: set --url, GITFAR_URL or url in config.toml",
                2,
            )
        })?;
    let token = env("GITFAR_TOKEN").or(file.token);
    let base = url.trim_end_matches('/');
    let http = reqwest::blocking::Client::builder()
        .timeout(None)
        .build()
        .map_err(|e| Fail::new("network", e, 6))?;
    let auth = |r: reqwest::blocking::RequestBuilder| match &token {
        Some(t) => r.bearer_auth(t),
        None => r,
    };

    if let Cmd::Whoami = cli.cmd {
        let v = send(auth(http.get(format!("{base}/v1/whoami"))), cli.json)?;
        if !cli.json {
            let scope = match v["scope"].as_array() {
                None => "global".to_owned(),
                Some(s) => s
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            println!(
                "token_id: {}\nscope: {scope}",
                v["token_id"].as_str().unwrap_or_default()
            );
        }
        return Ok(0);
    }

    let (prim, body) = request(cli.cmd);
    let endpoint = format!("{base}/v1/{}", prim.name());
    let deadline = Instant::now() + Duration::from_secs(cli.wait);
    // The last `repo_warming`, once retrying.
    let mut warming: Option<SendError> = None;
    let v = loop {
        let mut req = auth(http.post(&endpoint).json(&body));
        if warming.is_some() {
            // A retry must not run past --wait either; the last one, sent at the deadline, gets 1 s.
            let left = deadline.saturating_duration_since(Instant::now());
            req = req.timeout(left.max(Duration::from_secs(1)));
        }
        match send(req, cli.json) {
            Err(SendError::TimedOut) if warming.is_some() => {
                return Err(warming.expect("checked").into())
            }
            Err(w @ Warming { .. }) if deadline <= Instant::now() => return Err(w.into()),
            Err(w @ Warming { .. }) => {
                if let Warming {
                    retry_after,
                    progress,
                    ..
                } = &w
                {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let pause = (*retry_after).min(left);
                    eprintln!(
                        "gitfar: repo warming ({progress}); retrying in {}s",
                        pause.as_secs_f32().ceil()
                    );
                    std::thread::sleep(pause);
                }
                warming = Some(w);
            }
            other => break other?,
        }
    };
    let resp: Response<NoMeta> = serde_json::from_value(v.clone())
        .map_err(|e| Fail::new("network", format!("bad response: {e}"), 6))?;
    if !cli.json {
        print!("{}", resp.text);
        if !resp.text.is_empty() && !resp.text.ends_with('\n') {
            println!();
        }
        match &resp.next {
            Some(next) if resp.truncated => {
                eprintln!(
                    "[resolved_commit={} truncated=true next={next}]",
                    resp.resolved_commit
                )
            }
            _ => eprintln!("[resolved_commit={}]", resp.resolved_commit),
        }
    }
    // grep: no match (empty, complete result) exits 1 like grep(1).
    Ok(u8::from(
        prim == Primitive::Grep && resp.text.is_empty() && !resp.truncated,
    ))
}

use SendError::Warming;

enum SendError {
    Warming {
        retry_after: Duration,
        progress: String,
        message: String,
        /// Raw error body, echoed to stdout under `--json` once retries are over.
        body: Option<String>,
    },
    Fail(Fail),
    /// The request's own timeout ran out.
    TimedOut,
}

impl From<SendError> for Fail {
    fn from(e: SendError) -> Fail {
        match e {
            SendError::Fail(f) => f,
            SendError::TimedOut => Fail::new("network", "request timed out", 6),
            Warming { message, body, .. } => {
                if let Some(b) = body {
                    println!("{b}");
                }
                Fail::new("repo_warming", message, 5)
            }
        }
    }
}

/// One HTTP call. With `json`, the body (success or error) is echoed to stdout.
fn send(req: reqwest::blocking::RequestBuilder, json: bool) -> Result<Value, SendError> {
    let net = |e: reqwest::Error| match e.is_timeout() {
        true => SendError::TimedOut,
        false => SendError::Fail(Fail::new("network", e, 6)),
    };
    let r = req.send().map_err(net)?;
    let status = r.status();
    let retry_after = r
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok()?.trim().parse().ok())
        .unwrap_or(5u64)
        .max(1);
    let text = r.text().map_err(net)?;
    let v: Option<Value> = serde_json::from_str(&text).ok();
    if status.is_success() {
        let v =
            v.ok_or_else(|| SendError::Fail(Fail::new("network", "response is not JSON", 6)))?;
        if json {
            println!("{text}");
        }
        return Ok(v);
    }
    let Some(e) = v.and_then(|v| serde_json::from_value::<ErrorBody>(v).ok()) else {
        return Err(SendError::Fail(Fail::new(
            "network",
            format!("HTTP {status}"),
            6,
        )));
    };
    let d = e.error;
    if d.code == ErrorCode::RepoWarming {
        let progress = match &d.progress {
            Some(p) => format!(
                "{}{}",
                serde_json::to_value(p.state).unwrap().as_str().unwrap(),
                p.queue_position
                    .map(|q| format!(", queue position {q}"))
                    .unwrap_or_default()
            ),
            None => d.message.clone(),
        };
        return Err(Warming {
            retry_after: Duration::from_secs(retry_after),
            progress,
            message: d.message,
            body: json.then_some(text),
        });
    }
    if json {
        println!("{text}");
    }
    let code = serde_json::to_value(d.code).unwrap();
    Err(SendError::Fail(Fail::new(
        code.as_str().unwrap(),
        d.message,
        d.code.exit_code(),
    )))
}
