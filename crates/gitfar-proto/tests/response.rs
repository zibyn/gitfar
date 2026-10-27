use gitfar_proto::*;
use serde_json::json;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn untruncated_response_omits_reason_and_cursor() {
    let r: LogResponse = Response {
        text: "abc\n".into(),
        resolved_commit: SHA.into(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: NoMeta {},
    };
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        json!({ "text": "abc\n", "resolved_commit": SHA, "truncated": false })
    );
}

#[test]
fn truncated_response_flattens_primitive_metadata() {
    let r: GrepResponse = Response {
        text: "a.rs:1:x\n".into(),
        resolved_commit: SHA.into(),
        truncated: true,
        truncated_reason: Some(TruncatedReason::Deadline),
        next: Some("CURSOR".into()),
        meta: GrepMeta {
            skipped_large: vec!["big.bin".into()],
        },
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v,
        json!({
            "text": "a.rs:1:x\n", "resolved_commit": SHA, "truncated": true,
            "truncated_reason": "deadline", "next": "CURSOR", "skipped_large": ["big.bin"]
        })
    );
    assert_eq!(serde_json::from_value::<GrepResponse>(v).unwrap(), r);
}

#[test]
fn binary_read_reports_binary_and_size() {
    let r: ReadResponse = Response {
        text: String::new(),
        resolved_commit: SHA.into(),
        truncated: false,
        truncated_reason: None,
        next: None,
        meta: ReadMeta {
            total_lines: None,
            binary: true,
            size: Some(42),
        },
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["binary"], json!(true));
    assert_eq!(v["size"], json!(42));
    assert!(v.get("total_lines").is_none());
}

#[test]
fn text_read_reports_total_lines_without_binary_flag() {
    let v = serde_json::to_value(ReadMeta {
        total_lines: Some(7),
        binary: false,
        size: None,
    })
    .unwrap();
    assert_eq!(v, json!({ "total_lines": 7 }));
}
