//! `list`: `ls`/`find` over the tree, in `git ls-tree -r -t` order (spec §2.3).

use gitfar_proto::{
    ErrorBody, ListParams, ListResponse, ListType, NoMeta, Resume, TruncatedReason,
};
use globset::{GlobBuilder, GlobMatcher};

use super::{internal, invalid, lookup, next, Node, BYTE_CAP};

const MAX_ENTRIES: usize = 1000;

pub(super) fn run(
    commit: &gix::Commit<'_>,
    params: ListParams,
    skipped: u64,
) -> Result<ListResponse, ErrorBody> {
    let max_depth = params.max_depth.unwrap_or(1);
    if max_depth == 0 {
        return Err(invalid("max_depth must be >= 1"));
    }
    let glob = match &params.glob {
        Some(g) => Some(
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(invalid)?
                .compile_matcher(),
        ),
        None => None,
    };
    let mut out = Out {
        glob,
        ty: params.r#type.unwrap_or(ListType::Any),
        lines: Vec::new(),
    };
    let base = params.path.as_deref().unwrap_or("").trim_matches('/');
    match lookup(commit, base)? {
        Node::Dir(tree) => {
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            walk(&tree, &prefix, 1, max_depth, &mut out)?;
        }
        Node::Submodule(id) => out.push(base, Kind::Submodule(id)),
        Node::File(_) => out.push(base, Kind::File),
    }

    // shortcut: every page re-walks the whole subtree; stream with early exit if huge trees get slow.
    let mut resp = ListResponse {
        text: String::new(),
        resolved_commit: commit.id.to_string(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: NoMeta {},
    };
    let rest = out.lines.get(skipped as usize..).unwrap_or_default();
    let mut emitted = 0;
    for line in rest.iter().take(MAX_ENTRIES) {
        if emitted > 0 && resp.text.len() + line.len() > BYTE_CAP {
            resp.truncated_reason = Some(TruncatedReason::Bytes);
            break;
        }
        resp.text += line;
        emitted += 1;
    }
    if resp.truncated_reason.is_none() && emitted < rest.len() {
        resp.truncated_reason = Some(TruncatedReason::Limit);
    }
    if resp.truncated_reason.is_some() {
        resp.truncated = true;
        let skipped = skipped + emitted as u64;
        resp.next = next(commit.id, Resume::List { params, skipped });
    }
    Ok(resp)
}

enum Kind {
    File,
    Dir,
    Submodule(gix::ObjectId),
}

struct Out {
    glob: Option<GlobMatcher>,
    ty: ListType,
    lines: Vec<String>,
}

impl Out {
    fn push(&mut self, path: &str, kind: Kind) {
        let ty_ok = match (self.ty, &kind) {
            (ListType::Any, _) => true,
            // A submodule is a directory in a checkout.
            (ListType::Dir, k) => !matches!(k, Kind::File),
            (ListType::File, k) => matches!(k, Kind::File),
        };
        if !ty_ok || self.glob.as_ref().is_some_and(|g| !g.is_match(path)) {
            return;
        }
        self.lines.push(match kind {
            Kind::File => format!("{path}\n"),
            Kind::Dir => format!("{path}/\n"),
            Kind::Submodule(id) => format!("{path}@ {}\n", &id.to_string()[..12]),
        });
    }
}

/// Pre-order walk in stored tree order, which is git's order (`ls-tree -r -t`).
fn walk(
    tree: &gix::Tree<'_>,
    prefix: &str,
    depth: u32,
    max_depth: u32,
    out: &mut Out,
) -> Result<(), ErrorBody> {
    for e in tree.iter() {
        let e = e.map_err(internal)?;
        let path = format!("{prefix}{}", e.filename());
        let mode = e.mode();
        if mode.is_tree() {
            out.push(&path, Kind::Dir);
            if depth < max_depth {
                let sub = e.object().map_err(internal)?.into_tree();
                walk(&sub, &format!("{path}/"), depth + 1, max_depth, out)?;
            }
        } else if mode.is_commit() {
            out.push(&path, Kind::Submodule(e.object_id()));
        } else {
            out.push(&path, Kind::File);
        }
    }
    Ok(())
}
