//! Worker response schemas (RFC-0013 workers × RFC-0007 amendment
//! A-0007-2): every declared schema VALIDATES the samples the worker's
//! parser accepts and REJECTS malformed samples.
//!
//! The workspace dependency policy pins a deliberately small, curated set
//! (Cargo.toml workspace table; RFC-0015 T9 allow-list pattern), so instead
//! of pulling a JSON-Schema crate this file hand-rolls the minimal
//! structural subset the worker schemas actually use: `type`, `properties`,
//! `required`, `items`, `enum`, `additionalProperties: false`, and nullable
//! `type` arrays.
//!
//! Author: arkadianet

use alloy_runtime::capabilities::{
    edit_response_schema, repair_response_schema, review_response_schema,
};
use serde_json::{json, Value};

// --- minimal structural validator ---------------------------------------

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        other => panic!("unsupported type keyword {other:?}"),
    }
}

/// Validate `value` against the structural subset used by worker schemas.
fn validates(schema: &Value, value: &Value) -> bool {
    let obj = schema.as_object().expect("schema must be an object");

    if let Some(types) = obj.get("type") {
        let ok = match types {
            Value::String(ty) => type_matches(ty, value),
            Value::Array(list) => list
                .iter()
                .any(|ty| type_matches(ty.as_str().expect("type entry"), value)),
            other => panic!("unsupported type spec {other:?}"),
        };
        if !ok {
            return false;
        }
    }

    if let Some(allowed) = obj.get("enum") {
        let allowed = allowed.as_array().expect("enum must be an array");
        if !allowed.contains(value) {
            return false;
        }
    }

    if let Some(props) = obj.get("properties") {
        let props = props.as_object().expect("properties must be an object");
        let Some(map) = value.as_object() else {
            return false;
        };
        for (key, subschema) in props {
            if let Some(sub) = map.get(key) {
                if !validates(subschema, sub) {
                    return false;
                }
            }
        }
        if obj.get("additionalProperties") == Some(&Value::Bool(false))
            && map.keys().any(|key| !props.contains_key(key))
        {
            return false;
        }
        if let Some(required) = obj.get("required") {
            for key in required.as_array().expect("required must be an array") {
                if !map.contains_key(key.as_str().expect("required entry")) {
                    return false;
                }
            }
        }
    }

    if let Some(items) = obj.get("items") {
        if let Some(list) = value.as_array() {
            if !list.iter().all(|item| validates(items, item)) {
                return false;
            }
        }
    }

    true
}

// --- shared shape checks -------------------------------------------------

/// Every worker schema is a closed object schema a grammar-constrained
/// server can compile: a top-level `"type": "object"` with
/// `additionalProperties: false` and a non-empty `required` list.
#[test]
fn schemas_are_closed_object_schemas_with_stable_names() {
    for (spec, expected_name) in [
        (repair_response_schema(), "repair_plan"),
        (edit_response_schema(), "edit_patch"),
        (review_response_schema(), "review_report"),
    ] {
        assert_eq!(spec.name, expected_name);
        let schema = spec.schema.as_object().expect("schema object");
        assert_eq!(
            schema.get("type"),
            Some(&json!("object")),
            "{expected_name}"
        );
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&json!(false)),
            "{expected_name}"
        );
        assert!(
            schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|r| !r.is_empty()),
            "{expected_name} must require fields"
        );
    }
}

// --- repair --------------------------------------------------------------

fn repair_sample() -> Value {
    json!({
        "summary": "clone before the borrow",
        "target_files": ["src/lib.rs"],
        "steps": [
            { "file": "src/lib.rs", "rationale": "clone x", "anchor_line": 12 },
            { "file": "src/lib.rs", "rationale": "drop the ref", "anchor_line": null }
        ],
        "needs_replan": false,
        "confidence": 0.8
    })
}

#[test]
fn repair_schema_validates_parser_accepted_samples() {
    let schema = repair_response_schema().schema;
    assert!(validates(&schema, &repair_sample()));
    // Optional fields (serde defaults) may be omitted entirely.
    assert!(validates(
        &schema,
        &json!({
            "summary": "s",
            "target_files": [],
            "steps": []
        })
    ));
    // Nullable confidence.
    assert!(validates(
        &schema,
        &json!({
            "summary": "s",
            "target_files": ["a.rs"],
            "steps": [],
            "confidence": null
        })
    ));
}

#[test]
fn repair_schema_rejects_malformed_samples() {
    let schema = repair_response_schema().schema;
    // Missing required summary.
    assert!(!validates(
        &schema,
        &json!({ "target_files": [], "steps": [] })
    ));
    // Wrong-typed target_files.
    assert!(!validates(
        &schema,
        &json!({ "summary": "s", "target_files": "src/lib.rs", "steps": [] })
    ));
    // Unknown top-level key (parser is deny_unknown_fields).
    assert!(!validates(
        &schema,
        &json!({ "summary": "s", "target_files": [], "steps": [], "patch": "---" })
    ));
    // Step missing its rationale.
    assert!(!validates(
        &schema,
        &json!({
            "summary": "s",
            "target_files": [],
            "steps": [{ "file": "a.rs" }]
        })
    ));
}

// --- edit ----------------------------------------------------------------

#[test]
fn edit_schema_validates_and_rejects() {
    let schema = edit_response_schema().schema;
    assert!(validates(
        &schema,
        &json!({
            "patch": "--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n",
            "summary": "fix the type",
            "confidence": 0.7
        })
    ));
    // confidence omitted (serde default).
    assert!(validates(&schema, &json!({ "patch": "p", "summary": "s" })));
    // Ops form (AM-0013-1 / PR #64) — schema admits both keys; either/or
    // is enforced by the worker parser (provider schemas lack portable oneOf).
    assert!(validates(
        &schema,
        &json!({
            "ops": [{ "op": "replace_lines" }],
            "summary": "s"
        })
    ));
    // Summary alone is schema-valid (required list is just summary); the
    // parser still rejects neither-patch-nor-ops.
    assert!(validates(&schema, &json!({ "summary": "s" })));
    // Missing required summary.
    assert!(!validates(&schema, &json!({ "patch": "p" })));
    // Unknown key.
    assert!(!validates(
        &schema,
        &json!({ "patch": "p", "summary": "s", "files": [] })
    ));
    // Wrong-typed confidence.
    assert!(!validates(
        &schema,
        &json!({ "patch": "p", "summary": "s", "confidence": "high" })
    ));
}

// --- review --------------------------------------------------------------

#[test]
fn review_schema_validates_and_rejects() {
    let schema = review_response_schema().schema;
    assert!(validates(
        &schema,
        &json!({
            "verdict": "approve",
            "findings": [
                { "severity": "warning", "file": "src/a.rs", "line": 3, "message": "m" },
                { "severity": "info", "file": "src/b.rs", "line": null, "message": "n" }
            ],
            "summary": "ok",
            "confidence": 0.9
        })
    ));
    // findings/confidence omitted (serde defaults).
    assert!(validates(
        &schema,
        &json!({ "verdict": "request_changes", "summary": "s" })
    ));
    // Verdict outside the enum.
    assert!(!validates(
        &schema,
        &json!({ "verdict": "maybe", "summary": "s" })
    ));
    // Finding with a severity outside the enum.
    assert!(!validates(
        &schema,
        &json!({
            "verdict": "approve",
            "summary": "s",
            "findings": [{ "severity": "fatal", "file": "a.rs", "message": "m" }]
        })
    ));
    // Finding missing its message.
    assert!(!validates(
        &schema,
        &json!({
            "verdict": "approve",
            "summary": "s",
            "findings": [{ "severity": "info", "file": "a.rs" }]
        })
    ));
}

// --- validator self-test -------------------------------------------------

/// The hand-rolled subset itself must not be vacuously permissive.
#[test]
fn structural_validator_is_not_vacuous() {
    let schema = json!({
        "type": "object",
        "properties": { "a": { "type": "string" } },
        "required": ["a"],
        "additionalProperties": false
    });
    assert!(validates(&schema, &json!({ "a": "x" })));
    assert!(!validates(&schema, &json!({ "a": 1 })));
    assert!(!validates(&schema, &json!({})));
    assert!(!validates(&schema, &json!({ "a": "x", "b": 1 })));
    assert!(!validates(&schema, &json!("not an object")));
}
