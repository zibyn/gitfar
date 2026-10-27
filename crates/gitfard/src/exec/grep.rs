//! `grep`: per-file `search_slice` on the scan pool, merged in path + line order (spec §2.3, §4.6).
//!
//! Paging: the cursor holds the last emitted `(path, line)`; a resume skips everything at or
//! before it, so pages concatenate to exactly the unpaged output. Files/count entries use
//! line `u32::MAX` ("whole file done"); a deadline cut that emitted nothing yet uses
//! `NOTHING_YET` so the next page knows not to open with a `--`.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gitfar_proto::{
    ErrorBody, ErrorCode, GrepMeta, GrepOutputMode, GrepParams, GrepResponse, Resume,
    TruncatedReason,
};
use gix::ObjectId;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use rayon::prelude::*;

use super::{internal, invalid, next, BYTE_CAP};
use crate::admin::METRICS;

const DEFAULT_RESULTS: u32 = 100;
const LARGE: u64 = 10_000_000;
const LINE_CHARS: usize = 500;
/// Files per parallel batch; limits and the byte cap are checked between batches, the deadline per file.
const CHUNK: usize = 1024;
const NOTHING_YET: u32 = u32::MAX - 1;

/// The scan pool and the blob LRU, shared by all requests.
struct Engine {
    pool: rayon::ThreadPool,
    blobs: quick_cache::sync::Cache<ObjectId, Arc<Vec<u8>>, ByLen>,
    max_object: u64,
}

#[derive(Clone)]
struct ByLen;
impl quick_cache::Weighter<ObjectId, Arc<Vec<u8>>> for ByLen {
    fn weight(&self, _: &ObjectId, v: &Arc<Vec<u8>>) -> u64 {
        v.len() as u64
    }
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Sizes the scan pool (`0` = cores) and the blob LRU (`None` = 1/4 of RAM). Restart-only
/// settings, so only the first call counts; without one, defaults are used.
pub fn init(threads: usize, lru_budget: Option<u64>, max_object: u64) {
    let _ = ENGINE.set(Engine::new(threads, lru_budget, max_object));
}

/// Blob LRU bytes held and budget, and scan pool size (for `/metrics`).
pub(crate) fn stats() -> (u64, u64, usize) {
    let e = engine();
    (
        e.blobs.weight(),
        e.blobs.capacity(),
        e.pool.current_num_threads(),
    )
}

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| Engine::new(0, None, 8_000_000))
}

impl Engine {
    fn new(threads: usize, budget: Option<u64>, max_object: u64) -> Engine {
        let budget = budget.unwrap_or_else(|| mem_total() / 4).max(1);
        Engine {
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("grep-{i}"))
                .build()
                .expect("build scan pool"),
            // Item estimate only sizes the hash table; ~64 KB average blob.
            blobs: quick_cache::sync::Cache::with_weighter(
                (budget / 65536).clamp(16, 1 << 20) as usize,
                budget,
                ByLen,
            ),
            max_object,
        }
    }
}

/// shortcut: MemTotal ignores cgroup limits; read memory.max if containers get OOM-killed.
fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            let kb = m.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
            kb.trim().trim_end_matches("kB").trim().parse::<u64>().ok()
        })
        .map_or(4 << 30, |kb| kb * 1024)
}

struct File {
    path: String,
    oid: ObjectId,
}

/// One output line of a file; `sep` = a `--` goes before it if anything was printed before.
struct Entry {
    line: u32,
    is_match: bool,
    sep: bool,
    text: String,
}

enum Scanned {
    /// Not scanned: the deadline had passed.
    Cut,
    Large,
    Entries(Vec<Entry>),
}

pub(super) fn run(
    commit: &gix::Commit<'_>,
    params: GrepParams,
    after: (String, u32),
    deadline: Instant,
) -> Result<GrepResponse, ErrorBody> {
    let mode = params.output_mode.unwrap_or(GrepOutputMode::Lines);
    let limit = params.max_results.unwrap_or(DEFAULT_RESULTS);
    if limit == 0 {
        return Err(invalid("max_results must be >= 1"));
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(params.ignore_case.unwrap_or(false))
        .fixed_strings(params.fixed_string.unwrap_or(false))
        .line_terminator(Some(b'\n'))
        .build(&params.pattern)
        .map_err(invalid)?;
    let (before, after_ctx) = match mode {
        GrepOutputMode::Lines => (
            params.before.or(params.context).unwrap_or(0) as usize,
            params.after.or(params.context).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    };
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .before_context(before)
        .after_context(after_ctx)
        // git's rule is applied per file below.
        .binary_detection(BinaryDetection::none());
    let context = before + after_ctx > 0;
    let include = globs(&params.include)?;
    let exclude = globs(&params.exclude)?;
    let prefixes: Vec<&str> = params.path.iter().map(|p| p.trim_matches('/')).collect();
    let wanted = |p: &str| {
        (prefixes.is_empty()
            || prefixes.iter().any(|x| {
                x.is_empty() || p == *x || p.strip_prefix(x).is_some_and(|r| r.starts_with('/'))
            }))
            && (params.include.is_empty() || include.is_match(p))
            && !exclude.is_match(p)
    };

    // shortcut: walks the whole tree even under a path prefix; descend into it if that shows in profiles.
    let mut rec = gix::traverse::tree::Recorder::default();
    commit
        .tree()
        .map_err(internal)?
        .traverse()
        .breadthfirst(&mut rec)
        .map_err(internal)?;
    // Regular and executable blobs only: no symlinks, no submodules (as `git grep`).
    let mut files: Vec<File> = rec
        .records
        .into_iter()
        .filter(|e| e.mode.is_blob())
        .map(|e| File {
            path: e.filepath.to_string(),
            oid: e.oid,
        })
        .filter(|f| wanted(&f.path))
        .collect();
    // Bytewise full-path order is git's tree order.
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    // A cursor ending on a whole file (deadline cut, files/count entry) starts after it.
    let start = if after.1 >= NOTHING_YET {
        files.partition_point(|f| f.path <= after.0)
    } else {
        files.partition_point(|f| f.path < after.0)
    };

    let engine = engine();
    let sync = commit.repo.clone().into_sync();
    let skip_large = params.skip_large.unwrap_or(true);
    let mut resp = GrepResponse {
        text: String::new(),
        resolved_commit: commit.id.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: GrepMeta::default(),
    };
    let fresh = after.0.is_empty() && after.1 == 0;
    let mut printed = !fresh && after.1 != NOTHING_YET;
    let (mut matches, mut emitted) = (0u32, 0usize);
    let mut last = after.clone();
    let first = files.get(start).map(|f| f.path.as_str());
    let mut prev: Option<&str> = None;
    'chunks: for chunk in files[start..].chunks(CHUNK) {
        let scanned: Vec<Result<Scanned, ErrorBody>> = engine.pool.install(|| {
            chunk
                .par_iter()
                .map_init(
                    || (sync.to_thread_local(), builder.build()),
                    |(repo, searcher), f| {
                        // The page's first file always completes, so every page makes progress.
                        if Some(f.path.as_str()) != first && Instant::now() >= deadline {
                            return Ok(Scanned::Cut);
                        }
                        let how = How {
                            matcher: &matcher,
                            mode,
                            context,
                            skip_large,
                        };
                        METRICS.scan_busy.inc();
                        let res = scan(engine, repo, searcher, &how, f);
                        METRICS.scan_busy.dec();
                        res
                    },
                )
                .collect()
        });
        for (f, s) in chunk.iter().zip(scanned) {
            let entries = match s? {
                Scanned::Cut => {
                    resp.truncated_reason = Some(TruncatedReason::Deadline);
                    let done = prev.expect("the first file is never cut").to_owned();
                    last = (done, if printed { u32::MAX } else { NOTHING_YET });
                    break 'chunks;
                }
                Scanned::Large => {
                    prev = Some(&f.path);
                    if f.path != after.0 {
                        resp.meta.skipped_large.push(f.path.clone());
                    }
                    continue;
                }
                Scanned::Entries(e) => e,
            };
            for e in entries {
                if f.path == after.0 && e.line <= after.1 {
                    continue;
                }
                if matches == limit && (e.is_match || e.sep) {
                    resp.truncated_reason = Some(TruncatedReason::Limit);
                    break 'chunks;
                }
                let sep = if e.sep && printed { "--\n" } else { "" };
                if emitted > 0 && resp.text.len() + sep.len() + e.text.len() > BYTE_CAP {
                    resp.truncated_reason = Some(TruncatedReason::Bytes);
                    break 'chunks;
                }
                resp.text += sep;
                resp.text += &e.text;
                last = (f.path.clone(), e.line);
                printed = true;
                emitted += 1;
                matches += u32::from(e.is_match);
            }
            prev = Some(&f.path);
        }
    }
    if resp.truncated_reason.is_some() {
        resp.truncated = true;
        resp.next = next(
            commit.id,
            Resume::Grep {
                params,
                path: last.0,
                line: last.1,
            },
        );
    }
    Ok(resp)
}

fn globs(patterns: &[String]) -> Result<GlobSet, ErrorBody> {
    let mut set = GlobSetBuilder::new();
    for p in patterns {
        // git `:(glob)` semantics: `*` stays within a path component, `**` spans them.
        set.add(
            GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(invalid)?,
        );
    }
    set.build().map_err(invalid)
}

struct How<'a> {
    matcher: &'a RegexMatcher,
    mode: GrepOutputMode,
    context: bool,
    skip_large: bool,
}

fn scan(
    engine: &Engine,
    repo: &gix::Repository,
    searcher: &mut Searcher,
    how: &How<'_>,
    f: &File,
) -> Result<Scanned, ErrorBody> {
    let (mode, skip_large) = (how.mode, how.skip_large);
    let data = match engine.blobs.get(&f.oid) {
        Some(d) => {
            METRICS.blob_hits.inc();
            d
        }
        None => {
            METRICS.blob_misses.inc();
            if skip_large && repo.find_header(f.oid).map_err(internal)?.size() > LARGE {
                return Ok(Scanned::Large);
            }
            let d = Arc::new(repo.find_object(f.oid).map_err(internal)?.detach().data);
            if d.len() as u64 <= engine.max_object {
                engine.blobs.insert(f.oid, d.clone());
            }
            d
        }
    };
    if skip_large && data.len() as u64 > LARGE {
        return Ok(Scanned::Large);
    }
    // git's buffer_is_binary: a NUL in the first 8000 bytes; `git grep -I` skips those.
    if data[..data.len().min(8000)].contains(&0) {
        return Ok(Scanned::Entries(vec![]));
    }
    let mut sink = Collect {
        path: &f.path,
        mode,
        entries: vec![],
        count: 0,
        sep: how.context,
    };
    searcher
        .search_slice(how.matcher, &data, &mut sink)
        .map_err(internal)?;
    let mut entries = sink.entries;
    if sink.count > 0 {
        match mode {
            GrepOutputMode::Lines => {}
            GrepOutputMode::Files => entries.push(whole(format!("{}\n", f.path))),
            GrepOutputMode::Count => entries.push(whole(format!("{}:{}\n", f.path, sink.count))),
        }
    }
    Ok(Scanned::Entries(entries))
}

fn whole(text: String) -> Entry {
    Entry {
        line: u32::MAX,
        is_match: true,
        sep: false,
        text,
    }
}

struct Collect<'a> {
    path: &'a str,
    mode: GrepOutputMode,
    entries: Vec<Entry>,
    count: u64,
    /// A `--` is due before the next line (start of file, or a context break).
    sep: bool,
}

impl Collect<'_> {
    fn push(&mut self, n: Option<u64>, is_match: bool, bytes: &[u8]) {
        let n = n.unwrap_or(0) as u32;
        let line = String::from_utf8_lossy(bytes.strip_suffix(b"\n").unwrap_or(bytes));
        let line = match line.char_indices().nth(LINE_CHARS) {
            Some((i, _)) => &line[..i],
            None => &line,
        };
        let mark = if is_match { ':' } else { '-' };
        self.entries.push(Entry {
            line: n,
            is_match,
            sep: std::mem::take(&mut self.sep),
            text: format!("{}{mark}{n}{mark}{line}\n", self.path),
        });
    }
}

impl Sink for &mut Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.count += 1;
        match self.mode {
            GrepOutputMode::Lines => self.push(m.line_number(), true, m.bytes()),
            GrepOutputMode::Files => return Ok(false),
            GrepOutputMode::Count => {}
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.push(c.line_number(), false, c.bytes());
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.sep = true;
        Ok(true)
    }
}

/// Greps a local repo end to end, following cursors, for the manual benchmark
/// (`examples/grep_bench.rs`). Returns the concatenated text and the page count.
#[doc(hidden)]
pub fn grep_all(repo: &Path, rev: &str, params: GrepParams) -> Result<(String, usize), ErrorBody> {
    let repo = gix::open_opts(repo, gix::open::Options::isolated()).map_err(internal)?;
    let commit = repo
        .rev_parse_single(rev)
        .ok()
        .and_then(|id| id.object().ok()?.peel_to_commit().ok())
        .ok_or_else(|| ErrorBody::new(ErrorCode::RevNotFound, rev.to_owned()))?;
    let (mut text, mut pages, mut after) = (String::new(), 0, (String::new(), 0));
    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let r = run(&commit, params.clone(), after, deadline)?;
        text += &r.text;
        pages += 1;
        let Some(next) = r.next else {
            return Ok((text, pages));
        };
        match gitfar_proto::Cursor::decode(&next)?.resume {
            Resume::Grep { path, line, .. } => after = (path, line),
            _ => unreachable!("grep cursor"),
        }
    }
}
