use gitfar_proto::*;
use serde_json::{json, Value};

/// Sorted `required` list of an object schema.
fn required(schema: &Value) -> Vec<String> {
    let mut r: Vec<String> = serde_json::from_value(schema["required"].clone()).unwrap();
    r.sort();
    r
}

#[test]
fn every_primitive_has_a_name_and_strict_params_schema() {
    let names: Vec<_> = Primitive::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["read", "grep", "list", "log", "show", "blame"]);

    for p in Primitive::ALL {
        let s = p.params_schema().to_value();
        assert_eq!(s["type"], json!("object"), "{}", p.name());
        assert_eq!(s["additionalProperties"], json!(false), "{}", p.name());
        assert!(required(&s).contains(&"repo".to_string()), "{}", p.name());
    }
}

#[test]
fn params_schema_reflects_required_fields() {
    assert_eq!(
        required(&Primitive::Show.params_schema().to_value()),
        ["repo", "rev"]
    );
    assert_eq!(
        required(&Primitive::Blame.params_schema().to_value()),
        ["end_line", "path", "repo", "start_line"]
    );
    let s = Primitive::Grep.params_schema().to_value();
    assert!(s.to_string().contains("\"count\""), "output_mode enum: {s}");
}

#[test]
fn every_primitive_exports_a_response_schema() {
    for p in Primitive::ALL {
        let s = p.response_schema().to_value();
        assert_eq!(
            required(&s),
            ["resolved_commit", "text", "truncated"],
            "{}",
            p.name()
        );
    }
    let read = Primitive::Read.response_schema().to_value();
    assert!(read["properties"].get("total_lines").is_some(), "{read}");
}
