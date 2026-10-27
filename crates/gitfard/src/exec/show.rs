//! `show`: `git show <rev>` (header + diff) or `git diff <a> <b>` for `a..b` (spec §2.3).

use gitfar_proto::{
    ErrorBody, ErrorCode, NoMeta, Resume, ShowFormat, ShowParams, ShowResponse, TruncatedReason,
};
use gix::revision::plumbing::Spec;
use gix::ObjectId;

use super::git::{pathspecs, Git};
use super::{invalid, next, BYTE_CAP};

pub(super) fn run(
    repo: &gix::Repository,
    git: &Git,
    mut params: ShowParams,
    byte_offset: u64,
) -> Result<ShowResponse, ErrorBody> {
    let unknown = ErrorBody::new(
        ErrorCode::RevNotFound,
        format!("unknown revision {:?}", params.rev),
    );
    let commit = |id: ObjectId| {
        repo.find_object(id)
            .ok()
            .and_then(|o| o.peel_to_commit().ok())
            .map(|c| c.id)
            .ok_or_else(|| unknown.clone())
    };
    let spec = repo
        .rev_parse(params.rev.as_str())
        .map_err(|_| unknown.clone())?
        .detach();
    let (from, to) = match spec {
        Spec::Include(id) => (None, commit(id)?),
        Spec::Range { from, to } => (Some(commit(from)?), commit(to)?),
        _ => return Err(invalid("rev must be a single rev or an a..b range")),
    };
    // Pin both ends in the cursor; `resolved_commit` carries only the tip.
    params.rev = match from {
        Some(from) => format!("{from}..{to}"),
        None => to.to_string(),
    };

    let cmd = if from.is_some() { "diff" } else { "show" };
    // `-U` implies a patch, so it goes only with the patch format.
    let fmt = match params.format.unwrap_or(ShowFormat::Patch) {
        ShowFormat::Stat => "--stat".into(),
        ShowFormat::NameOnly => "--name-only".into(),
        ShowFormat::Patch => format!("-U{}", params.context_lines.unwrap_or(3)),
    };
    let mut args = vec![cmd.to_owned(), fmt, "--end-of-options".into()];
    args.extend(from.map(|f| f.to_string()));
    args.extend([to.to_string(), "--".into()]);
    args.extend(pathspecs(&params.path));

    // Deterministic for fixed SHAs, so a resume re-runs git and skips what was sent.
    let offset = usize::try_from(byte_offset).map_err(invalid)?;
    let out = git.run(&args, offset.saturating_add(BYTE_CAP + 1))?;
    let rest = out.get(offset..).unwrap_or_default();
    let mut resp = ShowResponse {
        text: String::new(),
        resolved_commit: to.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: NoMeta {},
    };
    let mut cut = rest.len();
    if cut > BYTE_CAP {
        // End the page on a line break, or failing that on a UTF-8 boundary.
        cut = match rest[..BYTE_CAP].iter().rposition(|&b| b == b'\n') {
            Some(nl) => nl + 1,
            None => (BYTE_CAP - 3..=BYTE_CAP)
                .rev()
                .find(|&i| rest[i] & 0xC0 != 0x80)
                .unwrap_or(BYTE_CAP),
        };
        resp.truncated = true;
        resp.truncated_reason = Some(TruncatedReason::Bytes);
        let byte_offset = byte_offset + cut as u64;
        resp.next = next(
            to,
            Resume::Show {
                params,
                byte_offset,
            },
        );
    }
    resp.text = String::from_utf8_lossy(&rest[..cut]).into_owned();
    Ok(resp)
}
