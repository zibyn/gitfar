//! `log`: `<sha12> <date> <author> <subject>` per commit, via `git log` (spec §2.3).

use gitfar_proto::{ErrorBody, LogParams, LogResponse, NoMeta, Resume, TruncatedReason};
use gix::ObjectId;

use super::git::{pathspecs, Git};
use super::{next, BYTE_CAP};

const DEFAULT_COUNT: u32 = 20;

pub(super) fn run(
    git: &Git,
    commit: ObjectId,
    params: LogParams,
    skipped: u64,
) -> Result<LogResponse, ErrorBody> {
    let count = u64::from(params.max_count.unwrap_or(DEFAULT_COUNT).max(1));
    // One extra commit tells whether more follow.
    let mut args = vec![
        "log".to_owned(),
        "--format=%H %ad %an %s".to_owned(),
        "--date=short".to_owned(),
        format!("--skip={skipped}"),
        format!("--max-count={}", count + 1),
    ];
    // Each value is one `--opt=value` argv, so a leading `-` stays part of the value.
    for (opt, v) in [
        ("since", &params.since),
        ("until", &params.until),
        ("author", &params.author),
        ("grep", &params.grep),
    ] {
        if let Some(v) = v {
            args.push(format!("--{opt}={v}"));
        }
    }
    args.extend([
        "--end-of-options".to_owned(),
        commit.to_string(),
        "--".to_owned(),
    ]);
    args.extend(pathspecs(&params.path));
    let out = git.run(&args, usize::MAX)?;
    let out = String::from_utf8_lossy(&out);

    let mut resp = LogResponse {
        text: String::new(),
        resolved_commit: commit.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: NoMeta {},
    };
    let mut emitted = 0;
    for line in out.split_inclusive('\n') {
        if emitted == count {
            resp.truncated_reason = Some(TruncatedReason::Limit);
            break;
        }
        // Full SHA cut to exactly 12 hex, where `%h` would grow on ambiguity.
        let line = format!("{}{}", &line[..12], &line[40..]);
        if emitted > 0 && resp.text.len() + line.len() > BYTE_CAP {
            resp.truncated_reason = Some(TruncatedReason::Bytes);
            break;
        }
        resp.text += &line;
        emitted += 1;
    }
    if resp.truncated_reason.is_some() {
        resp.truncated = true;
        let skipped = skipped + emitted;
        resp.next = next(commit, Resume::Log { params, skipped });
    }
    Ok(resp)
}
