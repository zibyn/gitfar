//! `blame`: `<sha12> <author> <date> <line>) <text>` per line, via `git blame -L` (spec §2.3).

use gitfar_proto::{
    BlameParams, BlameResponse, ErrorBody, ErrorCode, NoMeta, Resume, TruncatedReason,
};

use super::git::Runner;
use super::{internal, invalid, lookup, next, Node, BYTE_CAP};

const MAX_LINES: u32 = 500;

pub(super) fn run(
    commit: &gix::Commit<'_>,
    git: &Runner,
    params: BlameParams,
    next_line: u32,
) -> Result<BlameResponse, ErrorBody> {
    let (start, end) = (params.start_line, params.end_line);
    // `next_line` comes from an untrusted cursor.
    if start == 0 || end < start || !(start..=end).contains(&next_line) {
        return Err(invalid("need 1 <= start_line <= end_line"));
    }
    if end - start >= MAX_LINES {
        return Err(invalid(format!("blame covers at most {MAX_LINES} lines")));
    }
    let path = params.path.trim_matches('/');
    let data = match lookup(commit, path)? {
        Node::File(blob) => blob.detach().data,
        Node::Dir(_) | Node::Submodule(_) => {
            return Err(ErrorBody::new(
                ErrorCode::IsDirectory,
                format!("{path} is a directory"),
            ))
        }
    };
    let mut resp = BlameResponse {
        text: String::new(),
        resolved_commit: commit.id.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: NoMeta {},
    };
    // Like `read`, a range past EOF is clipped (git itself would fail).
    let total = data.split_inclusive(|&b| b == b'\n').count() as u32;
    let last = end.min(total);
    if next_line > last {
        return Ok(resp);
    }
    // Only the SHA precedes `--`; `--end-of-options` here would make git take the path as a rev.
    let args = [
        "blame".to_owned(),
        "--line-porcelain".to_owned(),
        format!("-L{next_line},{last}"),
        commit.id.to_string(),
        "--".to_owned(),
        path.to_owned(),
    ];
    let out = git.run(&args, usize::MAX)?;

    let (mut sha, mut line, mut author, mut time, mut tz) = (String::new(), 0, String::new(), 0, 0);
    for row in out.split(|&b| b == b'\n') {
        if let Some(text) = row.strip_prefix(b"\t") {
            let date = jiff::Timestamp::from_second(time)
                .and_then(|t| Ok(t.to_zoned(jiff::tz::Offset::from_seconds(tz)?.to_time_zone())))
                .map_err(internal)?
                .date();
            let text = String::from_utf8_lossy(text);
            let row = format!("{} {author} {date} {line}) {text}\n", &sha[..12]);
            if line > next_line && resp.text.len() + row.len() > BYTE_CAP {
                resp.truncated = true;
                resp.truncated_reason = Some(TruncatedReason::Bytes);
                resp.next = next(
                    commit.id,
                    Resume::Blame {
                        params,
                        next_line: line,
                    },
                );
                break;
            }
            resp.text += &row;
            continue;
        }
        // `summary`/`author` may be in any encoding.
        let row = String::from_utf8_lossy(row);
        if let Some(v) = row.strip_prefix("author ") {
            author = v.to_owned();
        } else if let Some(v) = row.strip_prefix("author-time ") {
            time = v.parse().map_err(internal)?;
        } else if let Some(v) = row.strip_prefix("author-tz ") {
            tz = tz_seconds(v).ok_or_else(|| internal(format!("bad author-tz {v:?}")))?;
        } else if row.len() > 41 && row.as_bytes()[40] == b' ' {
            // Header `<sha> <orig line> <final line> [<group size>]`.
            let mut f = row.split(' ');
            sha = f.next().unwrap_or_default().to_owned();
            line = f
                .nth(1)
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| internal(&row))?;
        }
    }
    Ok(resp)
}

/// `+0800` -> 28800.
fn tz_seconds(tz: &str) -> Option<i32> {
    let sign = match tz.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = (
        tz.get(1..3)?.parse::<i32>().ok()?,
        tz.get(3..5)?.parse::<i32>().ok()?,
    );
    Some(sign * (h * 3600 + m * 60))
}
