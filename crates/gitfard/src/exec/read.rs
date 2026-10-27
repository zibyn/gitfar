//! `read`: `cat -n` view of one blob (spec §2.3).

use gitfar_proto::{
    ErrorBody, ErrorCode, ReadMeta, ReadParams, ReadResponse, Resume, TruncatedReason,
};

use super::{internal, invalid, lookup, next, Node, BYTE_CAP};

const DEFAULT_LINES: usize = 200;
const MAX_LINES: usize = 2000;

pub(super) fn run(
    commit: &gix::Commit<'_>,
    params: ReadParams,
    next_line: u32,
) -> Result<ReadResponse, ErrorBody> {
    let start = params.start_line.unwrap_or(1);
    // `next_line` comes from an untrusted cursor.
    if start == 0 || next_line == 0 || params.end_line.is_some_and(|e| e < start) {
        return Err(invalid("need 1 <= start_line <= end_line"));
    }
    let data = match lookup(commit, &params.path)? {
        Node::File(blob) => blob.detach().data,
        Node::Dir(_) | Node::Submodule(_) => {
            return Err(ErrorBody::new(
                ErrorCode::IsDirectory,
                format!("{} is a directory", params.path),
            ))
        }
    };
    let mut resp = ReadResponse {
        text: String::new(),
        resolved_commit: commit.id.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: ReadMeta::default(),
    };
    // git's buffer_is_binary heuristic.
    if data[..data.len().min(8000)].contains(&0) {
        resp.meta.binary = true;
        resp.meta.size = Some(data.len() as u64);
        return Ok(resp);
    }
    let lines: Vec<&[u8]> = data.split_inclusive(|&b| b == b'\n').collect();
    resp.meta.total_lines = Some(lines.len() as u64);

    let first = next_line as usize;
    // Unbounded `end_line` pages through to EOF; the page size is the default, or the cap if given.
    let end = params
        .end_line
        .map_or(lines.len(), |e| lines.len().min(e as usize));
    let page = if params.end_line.is_some() {
        MAX_LINES
    } else {
        DEFAULT_LINES
    };
    let stop = end.min(first + page - 1);
    let mut n = first;
    let mut reason = None;
    while n <= stop {
        let line = format!("{n:>6}\t{}", String::from_utf8_lossy(lines[n - 1]));
        // Always emit one line, so a single huge line cannot stall the cursor.
        if n > first && resp.text.len() + line.len() > BYTE_CAP {
            reason = Some(TruncatedReason::Bytes);
            break;
        }
        resp.text += &line;
        n += 1;
    }
    if reason.is_none() && n <= end {
        reason = Some(TruncatedReason::Limit);
    }
    if reason.is_some() {
        resp.truncated = true;
        resp.truncated_reason = reason;
        let next_line = u32::try_from(n).map_err(internal)?;
        resp.next = next(commit.id, Resume::Read { params, next_line });
    }
    Ok(resp)
}
