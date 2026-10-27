use gitfar_proto::*;
use serde_json::json;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn one_of_each() -> Vec<Resume> {
    vec![
        Resume::Read {
            params: ReadParams {
                repo: "github.com/zibyn/gitfar".into(),
                rev: Some("main".into()),
                path: "src/lib.rs".into(),
                start_line: Some(1),
                end_line: None,
            },
            next_line: 201,
        },
        Resume::Grep {
            params: GrepParams {
                repo: "r".into(),
                rev: None,
                pattern: "fn \\w+".into(),
                path: vec!["src/".into()],
                include: vec!["**/*.rs".into()],
                exclude: vec![],
                fixed_string: None,
                ignore_case: Some(true),
                before: None,
                after: None,
                context: Some(2),
                output_mode: Some(GrepOutputMode::Lines),
                max_results: Some(50),
                skip_large: None,
            },
            path: "src/路径.rs".into(),
            line: 42,
        },
        Resume::List {
            params: ListParams {
                repo: "r".into(),
                rev: None,
                path: Some("crates".into()),
                glob: Some("*.toml".into()),
                max_depth: Some(3),
                r#type: Some(ListType::File),
            },
            skipped: 1000,
        },
        Resume::Log {
            params: LogParams {
                repo: "r".into(),
                rev: Some("v1.0".into()),
                path: vec!["README.md".into()],
                max_count: None,
                since: Some("2026-01-01".into()),
                until: None,
                author: Some("zibyn".into()),
                grep: None,
            },
            skipped: 20,
        },
        Resume::Show {
            params: ShowParams {
                repo: "r".into(),
                rev: "a..b".into(),
                path: vec![],
                format: Some(ShowFormat::Patch),
                context_lines: Some(5),
            },
            byte_offset: 65_536,
        },
        Resume::Blame {
            params: BlameParams {
                repo: "r".into(),
                rev: None,
                path: "src/main.rs".into(),
                start_line: 1,
                end_line: 500,
            },
            next_line: 251,
        },
    ]
}

fn cursor(resume: Resume) -> Cursor {
    Cursor {
        resolved_commit: SHA.into(),
        resume,
    }
}

#[test]
fn cursor_round_trips_for_every_primitive() {
    let resumes = one_of_each();
    let covered: Vec<Primitive> = resumes.iter().map(Resume::primitive).collect();
    assert_eq!(covered, Primitive::ALL);
    for resume in resumes {
        let c = cursor(resume);
        let token = c.encode();
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "not base64url: {token}"
        );
        assert_eq!(Cursor::decode(&token).unwrap(), c);
    }
}

fn assert_invalid(token: &str) {
    let err = Cursor::decode(token).expect_err(token);
    assert_eq!(err.error.code, ErrorCode::InvalidArgument, "{token}");
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[test]
fn corrupt_cursor_is_invalid_argument() {
    assert_invalid("");
    assert_invalid("not base64!");
    assert_invalid(&b64(b"\x01{\"resolved_commit\":"));
    assert_invalid(&b64(b"\x01null"));
}

#[test]
fn truncated_cursor_is_invalid_argument() {
    let good = cursor(one_of_each().remove(1)).encode();
    for len in [1, 2, 3, good.len() / 2, good.len() - 1] {
        assert_invalid(&good[..len]);
    }
}

#[test]
fn unknown_cursor_version_is_invalid_argument() {
    let good = cursor(one_of_each().remove(0)).encode();
    let mut raw = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(good)
            .unwrap()
    };
    for version in [0u8, 2, 255] {
        raw[0] = version;
        assert_invalid(&b64(&raw));
    }
}

#[test]
fn cursor_with_non_sha_commit_is_invalid_argument() {
    for commit in ["HEAD", "--output=/tmp/x", &SHA.to_uppercase(), &SHA[..39]] {
        let mut c = cursor(one_of_each().remove(0));
        c.resolved_commit = commit.into();
        assert_invalid(&c.encode());
    }
}

#[test]
fn cursor_only_request_parses_to_resume() {
    for resume in one_of_each() {
        let primitive = resume.primitive();
        let c = cursor(resume);
        let req = Request::parse(primitive, json!({ "cursor": c.encode() })).unwrap();
        assert_eq!(req, Request::Resume(c));
    }
}

#[test]
fn request_without_cursor_parses_to_fresh_params() {
    let req = Request::parse(Primitive::Read, json!({ "repo": "r", "path": "a" })).unwrap();
    let Request::Fresh(Params::Read(p)) = req else {
        panic!("{req:?}")
    };
    assert_eq!((p.repo.as_str(), p.path.as_str()), ("r", "a"));
    let err = Request::parse(Primitive::Read, json!({ "repo": "r" })).unwrap_err();
    assert_eq!(err.error.code, ErrorCode::InvalidArgument);
}

#[test]
fn cursor_with_other_params_is_invalid_argument() {
    let token = cursor(one_of_each().remove(0)).encode();
    for body in [
        json!({ "cursor": token, "repo": "r" }),
        json!({ "cursor": token, "rev": "main" }),
        json!({ "cursor": token, "repo": "r", "path": "a" }),
    ] {
        let err = Request::parse(Primitive::Read, body).unwrap_err();
        assert_eq!(err.error.code, ErrorCode::InvalidArgument);
    }
}

#[test]
fn bad_cursor_requests_are_invalid_argument() {
    let read_token = cursor(one_of_each().remove(0)).encode();
    for (primitive, body) in [
        (Primitive::Read, json!({ "cursor": "garbage" })),
        (Primitive::Read, json!({ "cursor": 42 })),
        (Primitive::Read, json!({ "cursor": null })),
        // A read cursor sent to /v1/grep.
        (Primitive::Grep, json!({ "cursor": read_token })),
    ] {
        let err = Request::parse(primitive, body).unwrap_err();
        assert_eq!(err.error.code, ErrorCode::InvalidArgument);
    }
}
