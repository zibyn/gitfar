use gitfar_proto::*;
use serde_json::json;

/// Spec §2.5, transcribed literally: (wire code, HTTP status, CLI exit code).
const SPEC_TABLE: &[(&str, u16, u8)] = &[
    ("invalid_argument", 400, 2),
    ("is_directory", 400, 2),
    ("unauthenticated", 401, 4),
    ("not_found", 404, 3),
    ("path_not_found", 404, 3),
    ("rev_not_found", 404, 3),
    ("repo_warming", 503, 5),
    ("cache_full", 507, 5),
    ("upstream_unavailable", 502, 5),
    ("deadline_exceeded", 504, 6),
    ("internal", 500, 6),
];

#[test]
fn every_error_code_maps_to_spec_http_status_and_exit_code() {
    for &(wire, http, exit) in SPEC_TABLE {
        let code: ErrorCode = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(code.http_status(), http, "{wire}");
        assert_eq!(code.exit_code(), exit, "{wire}");
        assert_eq!(serde_json::to_value(code).unwrap(), json!(wire));
    }
}

#[test]
fn spec_table_covers_every_error_code() {
    assert_eq!(ErrorCode::ALL.len(), SPEC_TABLE.len());
}

#[test]
fn error_body_serializes_as_spec_shape() {
    let body = ErrorBody::new(ErrorCode::PathNotFound, "no such path: src/x.rs");
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        json!({ "error": { "code": "path_not_found", "message": "no such path: src/x.rs" } })
    );
}

#[test]
fn repo_warming_body_carries_progress() {
    let mut body = ErrorBody::new(ErrorCode::RepoWarming, "warming");
    body.error.progress = Some(WarmingProgress {
        state: WarmingState::Queued,
        queue_position: Some(3),
    });
    let v = serde_json::to_value(&body).unwrap();
    assert_eq!(
        v,
        json!({ "error": {
            "code": "repo_warming",
            "message": "warming",
            "progress": { "state": "queued", "queue_position": 3 }
        }})
    );
    assert_eq!(serde_json::from_value::<ErrorBody>(v).unwrap(), body);
}
