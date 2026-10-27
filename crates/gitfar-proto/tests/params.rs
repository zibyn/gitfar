use gitfar_proto::*;
use serde_json::json;

#[test]
fn read_params_deserialize_from_json() {
    let p: ReadParams = serde_json::from_value(json!({
        "repo": "github.com/zibyn/gitfar",
        "path": "README.md",
        "start_line": 10,
        "end_line": 20
    }))
    .unwrap();
    assert_eq!(p.repo, "github.com/zibyn/gitfar");
    assert_eq!(p.rev, None);
    assert_eq!(p.path, "README.md");
    assert_eq!((p.start_line, p.end_line), (Some(10), Some(20)));
}

#[test]
fn unknown_field_is_rejected() {
    let err = serde_json::from_value::<ReadParams>(json!({
        "repo": "r", "path": "p", "line": 3
    }))
    .unwrap_err();
    assert!(err.to_string().contains("unknown field `line`"), "{err}");
}

#[test]
fn missing_required_field_is_rejected() {
    let err = serde_json::from_value::<ReadParams>(json!({ "repo": "r" })).unwrap_err();
    assert!(err.to_string().contains("missing field `path`"), "{err}");
}

#[test]
fn grep_params_deserialize_from_json() {
    let p: GrepParams = serde_json::from_value(json!({
        "repo": "r",
        "rev": "v6.1",
        "pattern": "fn main",
        "path": ["src/", "tests/"],
        "include": ["*.rs"],
        "exclude": ["**/gen/**"],
        "fixed_string": true,
        "ignore_case": false,
        "before": 1, "after": 2, "context": 3,
        "output_mode": "count",
        "max_results": 50,
        "skip_large": false
    }))
    .unwrap();
    assert_eq!(p.pattern, "fn main");
    assert_eq!(p.path, ["src/", "tests/"]);
    assert_eq!(p.include, ["*.rs"]);
    assert_eq!(p.exclude, ["**/gen/**"]);
    assert_eq!(p.fixed_string, Some(true));
    assert_eq!(p.output_mode, Some(GrepOutputMode::Count));
    assert_eq!(p.max_results, Some(50));
    assert_eq!(p.skip_large, Some(false));
    assert_eq!((p.before, p.after, p.context), (Some(1), Some(2), Some(3)));
}

#[test]
fn grep_minimal_params_leave_lists_empty() {
    let p: GrepParams = serde_json::from_value(json!({ "repo": "r", "pattern": "x" })).unwrap();
    assert!(p.path.is_empty() && p.include.is_empty() && p.exclude.is_empty());
    assert_eq!(p.output_mode, None);
}

#[test]
fn grep_rejects_unknown_output_mode_and_missing_pattern() {
    assert!(serde_json::from_value::<GrepParams>(json!({
        "repo": "r", "pattern": "x", "output_mode": "json"
    }))
    .is_err());
    assert!(serde_json::from_value::<GrepParams>(json!({ "repo": "r" })).is_err());
}

#[test]
fn list_params_deserialize_from_json() {
    let p: ListParams = serde_json::from_value(json!({
        "repo": "r", "path": "src", "glob": "**/*.rs", "max_depth": 3, "type": "dir"
    }))
    .unwrap();
    assert_eq!(p.path.as_deref(), Some("src"));
    assert_eq!(p.glob.as_deref(), Some("**/*.rs"));
    assert_eq!(p.max_depth, Some(3));
    assert_eq!(p.r#type, Some(ListType::Dir));
    assert!(serde_json::from_value::<ListParams>(json!({ "repo": "r" })).is_ok());
    assert!(serde_json::from_value::<ListParams>(json!({ "repo": "r", "type": "link" })).is_err());
    assert!(serde_json::from_value::<ListParams>(json!({ "path": "src" })).is_err());
}

#[test]
fn log_params_deserialize_from_json() {
    let p: LogParams = serde_json::from_value(json!({
        "repo": "r", "rev": "main", "path": ["a", "b"], "max_count": 5,
        "since": "2026-01-01", "until": "2026-10-01", "author": "zibyn", "grep": "fix"
    }))
    .unwrap();
    assert_eq!(p.path, ["a", "b"]);
    assert_eq!(p.max_count, Some(5));
    assert_eq!(p.since.as_deref(), Some("2026-01-01"));
    assert_eq!(p.until.as_deref(), Some("2026-10-01"));
    assert_eq!(p.author.as_deref(), Some("zibyn"));
    assert_eq!(p.grep.as_deref(), Some("fix"));
    assert!(serde_json::from_value::<LogParams>(json!({ "repo": "r", "patch": true })).is_err());
}

#[test]
fn show_params_deserialize_from_json() {
    let p: ShowParams = serde_json::from_value(json!({
        "repo": "r", "rev": "a..b", "path": ["src/"], "format": "name-only", "context_lines": 0
    }))
    .unwrap();
    assert_eq!(p.rev, "a..b");
    assert_eq!(p.path, ["src/"]);
    assert_eq!(p.format, Some(ShowFormat::NameOnly));
    assert_eq!(p.context_lines, Some(0));
    // rev is required for show
    let err = serde_json::from_value::<ShowParams>(json!({ "repo": "r" })).unwrap_err();
    assert!(err.to_string().contains("missing field `rev`"), "{err}");
}

#[test]
fn blame_params_require_line_range() {
    let p: BlameParams = serde_json::from_value(json!({
        "repo": "r", "path": "f", "start_line": 1, "end_line": 500
    }))
    .unwrap();
    assert_eq!((p.start_line, p.end_line), (1, 500));
    let err =
        serde_json::from_value::<BlameParams>(json!({ "repo": "r", "path": "f", "start_line": 1 }))
            .unwrap_err();
    assert!(
        err.to_string().contains("missing field `end_line`"),
        "{err}"
    );
}

#[test]
fn negative_line_numbers_are_rejected() {
    assert!(serde_json::from_value::<ReadParams>(json!({
        "repo": "r", "path": "p", "start_line": -1
    }))
    .is_err());
}
