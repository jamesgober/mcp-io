#![allow(clippy::unwrap_used)]

use super::*;
use crate::jsonx::{Value, json, parse};

fn schema(value: Value<'_>) -> Schema {
    Schema::compile(&value).unwrap()
}

fn refused(value: Value<'_>) -> SchemaError {
    Schema::compile(&value).unwrap_err()
}

#[test]
fn test_compile_rejects_non_schema_values() {
    let err = refused(json!(3));
    assert_eq!(err.pointer(), "");
    assert_eq!(err.kind(), &SchemaErrorKind::NotASchema { found: "number" });
    let err = refused(json!({ "properties": { "a": "string" } }));
    assert_eq!(err.pointer(), "/properties/a");
}

#[test]
fn test_compile_rejects_every_invalid_keyword_value() {
    let cases = [
        json!({ "type": "text" }),
        json!({ "type": [] }),
        json!({ "type": ["string", "string"] }),
        json!({ "enum": 3 }),
        json!({ "multipleOf": 0 }),
        json!({ "multipleOf": -2 }),
        json!({ "minimum": "1" }),
        json!({ "maxLength": -1 }),
        json!({ "maxLength": 1.5 }),
        json!({ "uniqueItems": "yes" }),
        json!({ "required": ["a", "a"] }),
        json!({ "required": [1] }),
        json!({ "dependentRequired": { "a": "b" } }),
        json!({ "allOf": [] }),
        json!({ "anyOf": {} }),
        json!({ "prefixItems": [] }),
        json!({ "items": [{}] }),
        json!({ "properties": [] }),
        json!({ "$defs": [] }),
        json!({ "$ref": 1 }),
        json!({ "$anchor": "1bad" }),
        json!({ "$id": 7 }),
    ];
    for case in cases {
        let text = case.to_string();
        let err = refused(case);
        assert!(
            matches!(err.kind(), SchemaErrorKind::InvalidKeyword { .. }),
            "{text}: {err}"
        );
    }
}

#[test]
fn test_compile_refuses_unsupported_keywords_and_dialects() {
    for keyword in [
        "pattern",
        "patternProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "$dynamicRef",
        "dependencies",
        "additionalItems",
    ] {
        let mut object = crate::jsonx::Object::new();
        object.push(keyword, json!({}));
        let err = refused(Value::Object(object));
        assert!(
            matches!(err.kind(), SchemaErrorKind::Unsupported { .. }),
            "{keyword}"
        );
    }
    let err = refused(json!({ "$schema": "http://json-schema.org/draft-07/schema#" }));
    assert!(matches!(
        err.kind(),
        SchemaErrorKind::UnsupportedDialect { .. }
    ));
    assert!(err.to_string().contains("only JSON Schema 2020-12"));
    let err = refused(json!({ "items": { "$id": "inner" } }));
    assert!(matches!(err.kind(), SchemaErrorKind::Unsupported { .. }));
    assert!(
        Schema::compile(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema#",
            "$id": "urn:x"
        }))
        .is_ok()
    );
    let err = refused(parse(r#"{"multipleOf": 0.12345678901234567890123}"#).unwrap());
    assert!(matches!(err.kind(), SchemaErrorKind::Unsupported { .. }));
}

#[test]
fn test_compile_refuses_references_it_cannot_resolve_locally() {
    assert!(matches!(
        refused(json!({ "$ref": "other.json" })).kind(),
        SchemaErrorKind::NonLocalRef { .. }
    ));
    assert!(matches!(
        refused(json!({ "$ref": "#/$defs/missing" })).kind(),
        SchemaErrorKind::UnresolvedRef { .. }
    ));
    assert!(matches!(
        refused(json!({ "$ref": "#nowhere" })).kind(),
        SchemaErrorKind::UnresolvedRef { .. }
    ));
    assert!(matches!(
        refused(json!({ "$ref": "#/bad%zz" })).kind(),
        SchemaErrorKind::UnresolvedRef { .. }
    ));
    let err = refused(json!({ "$defs": { "a": { "$anchor": "x" }, "b": { "$anchor": "x" } } }));
    assert!(matches!(
        err.kind(),
        SchemaErrorKind::DuplicateAnchor { .. }
    ));
}

#[test]
fn test_compile_budgets() {
    let mut deep = json!({});
    for _ in 0..70 {
        deep = json!({ "items": deep });
    }
    assert!(matches!(
        refused(deep).kind(),
        SchemaErrorKind::TooDeep { limit: 64 }
    ));
    let wide: crate::jsonx::Object<'_> = (0..10).map(|i| (format!("p{i}"), json!({}))).collect();
    let err = Schema::compile_with(
        &json!({ "properties": Value::Object(wide) }),
        SchemaLimits::new().with_max_nodes(5),
    )
    .unwrap_err();
    assert!(matches!(err.kind(), SchemaErrorKind::TooLarge { limit: 5 }));
}

#[test]
fn test_references_resolve_by_pointer_anchor_and_into_unknown_members() {
    let s = schema(json!({
        "$defs": { "id": { "$anchor": "id", "type": "integer" } },
        "definitions": { "name": { "type": "string" } },
        "properties": {
            "a": { "$ref": "#/$defs/id" },
            "b": { "$ref": "#id" },
            "c": { "$ref": "#/definitions/name" },
            "d": { "$ref": "#/properties/a" }
        }
    }));
    assert!(s.is_valid(&json!({ "a": 1, "b": 2, "c": "x", "d": 3 })));
    assert!(!s.is_valid(&json!({ "a": "1" })));
    assert!(!s.is_valid(&json!({ "b": 1.5 })));
    assert!(!s.is_valid(&json!({ "c": 1 })));
    assert!(!s.is_valid(&json!({ "d": "x" })));
}

#[test]
fn test_recursive_schema_validates_recursive_data() {
    let tree = schema(json!({
        "type": "object",
        "properties": { "children": { "type": "array", "items": { "$ref": "#" } } },
        "additionalProperties": false
    }));
    assert!(tree.is_valid(&json!({ "children": [{ "children": [] }, {}] })));
    assert!(!tree.is_valid(&json!({ "children": [{ "x": 1 }] })));
}

#[test]
fn test_self_reference_without_progress_exhausts_the_budget() {
    let looping = schema(json!({ "$ref": "#" }));
    let errors = looping.validate(&json!(1)).unwrap_err();
    assert!(errors.budget_exceeded());
    assert_eq!(errors.errors()[0].keyword(), "budget");
}

#[test]
fn test_step_budget_caps_combinatorial_work() {
    // Written out as a tree, this schema is too large to compile at all.
    let mut tree = json!({ "type": "integer" });
    for _ in 0..13 {
        tree = json!({ "anyOf": [{ "type": "string" }, tree.clone(), tree] });
    }
    assert!(matches!(
        refused(tree).kind(),
        SchemaErrorKind::TooLarge { .. }
    ));
    // Shared through `$ref`, it is small, and the step budget bounds the
    // 2^20 applications a failing instance would otherwise cost.
    let mut defs = crate::jsonx::Object::new();
    defs.push("d0", json!({ "type": "integer" }));
    for i in 1..=20 {
        let previous = format!("#/$defs/d{}", i - 1);
        defs.push(
            format!("d{i}"),
            json!({ "anyOf": [{ "type": "string" }, { "$ref": previous.clone() }, { "$ref": previous }] }),
        );
    }
    let s = Schema::compile_with(
        &json!({ "$defs": Value::Object(defs), "$ref": "#/$defs/d20" }),
        SchemaLimits::new().with_max_steps(10_000),
    )
    .unwrap();
    assert!(s.validate(&json!(true)).unwrap_err().budget_exceeded());
    assert!(s.is_valid(&json!(1)));
}

#[test]
fn test_errors_report_paths_keywords_and_never_values() {
    let s = schema(json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "minLength": 2 },
            "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 1 },
            "a~b/c": { "const": 1 }
        },
        "required": ["name", "id"],
        "additionalProperties": false
    }));
    let errors = s
        .validate(
            &json!({ "name": "s3cret-x", "tags": ["x", 4], "a~b/c": 2, "zzz": "hidden-value" }),
        )
        .unwrap_err();
    let report: Vec<(String, &str)> = errors
        .errors()
        .iter()
        .map(|e| (e.instance_path().to_owned(), e.keyword()))
        .collect();
    assert_eq!(
        report,
        [
            (String::new(), "required"),
            ("/tags".to_owned(), "maxItems"),
            ("/tags/1".to_owned(), "type"),
            ("/a~0b~1c".to_owned(), "const"),
            ("/zzz".to_owned(), "additionalProperties"),
        ]
    );
    let text = errors.to_string();
    assert!(
        !text.contains("hidden-value") && !text.contains("s3cret"),
        "{text}"
    );
    assert!(text.contains("\"id\""));
}

#[test]
fn test_error_count_is_capped() {
    let required: Vec<Value<'static>> = (0..40).map(|i| Value::from(format!("p{i}"))).collect();
    let s = Schema::compile_with(
        &json!({ "required": required }),
        SchemaLimits::new().with_max_errors(3),
    )
    .unwrap();
    let errors = s.validate(&json!({})).unwrap_err();
    assert_eq!(errors.errors().len(), 3);
    assert!(errors.truncated());
    assert!(errors.to_string().ends_with("(more errors not shown)"));
}

#[test]
fn test_numbers_compare_exactly() {
    let s = schema(json!({ "type": "integer", "maximum": 9007199254740992u64 }));
    assert!(s.is_valid(&parse("9007199254740992").unwrap()));
    assert!(!s.is_valid(&parse("9007199254740993").unwrap()));
    assert!(s.is_valid(&parse("2.0").unwrap()));
    let s = schema(parse(r#"{"multipleOf": 0.01}"#).unwrap());
    assert!(s.is_valid(&parse("19.99").unwrap()));
    assert!(!s.is_valid(&parse("19.999").unwrap()));
    let s = schema(json!({ "exclusiveMinimum": 0, "exclusiveMaximum": 1 }));
    assert!(s.is_valid(&parse("0.5").unwrap()));
    assert!(!s.is_valid(&json!(0)) && !s.is_valid(&json!(1)));
    let errors = schema(parse(r#"{"minimum": 1.5}"#).unwrap())
        .validate(&json!(1))
        .unwrap_err();
    assert_eq!(errors.errors()[0].message(), "must be at least 15e-1");
}

#[test]
fn test_strings_count_code_points() {
    let s = schema(json!({ "minLength": 2, "maxLength": 2 }));
    assert!(s.is_valid(&json!("\u{1F600}\u{1F600}")));
    assert!(!s.is_valid(&json!("\u{1F600}")));
    assert!(
        s.is_valid(&json!(12345)),
        "length keywords ignore other types"
    );
}

#[test]
fn test_contains_and_bounds() {
    let s = schema(json!({ "contains": { "const": 1 }, "minContains": 2, "maxContains": 3 }));
    assert!(s.is_valid(&json!([1, 1, 2])));
    assert!(!s.is_valid(&json!([1, 2])));
    assert!(!s.is_valid(&json!([1, 1, 1, 1])));
    let s = schema(json!({ "contains": { "const": 1 }, "minContains": 0 }));
    assert!(s.is_valid(&json!([])));
}

#[test]
fn test_object_dependencies_and_names() {
    let s = schema(json!({
        "dependentRequired": { "card": ["cvv"] },
        "dependentSchemas": { "gift": { "required": ["to"] } },
        "propertyNames": { "maxLength": 4 },
        "minProperties": 1,
        "maxProperties": 3
    }));
    assert!(s.is_valid(&json!({ "card": 1, "cvv": 2 })));
    assert!(!s.is_valid(&json!({ "card": 1 })));
    assert!(!s.is_valid(&json!({ "gift": 1 })));
    assert!(!s.is_valid(&json!({ "toolong": 1 })));
    assert!(!s.is_valid(&json!({})));
    assert!(!s.is_valid(&json!({ "a": 1, "b": 2, "c": 3, "d": 4 })));
    let errors = s.validate(&json!({ "toolong": 1 })).unwrap_err();
    assert_eq!(errors.errors()[0].keyword(), "propertyNames");
}

#[test]
fn test_composition_reports() {
    let one = schema(json!({ "oneOf": [{ "type": "integer" }, { "minimum": 0 }] }));
    assert!(one.is_valid(&json!(-1)));
    assert_eq!(
        one.validate(&json!(1)).unwrap_err().errors()[0].message(),
        "matches more than one of the alternatives in oneOf"
    );
    assert_eq!(
        one.validate(&parse("-1.5").unwrap()).unwrap_err().errors()[0].message(),
        "does not match any of the alternatives in oneOf"
    );
    let not = schema(json!({ "not": { "type": "null" } }));
    assert_eq!(
        not.validate(&json!(null)).unwrap_err().errors()[0].keyword(),
        "not"
    );
    let cond = schema(json!({
        "if": { "type": "string" },
        "then": { "minLength": 1 },
        "else": { "type": "integer" }
    }));
    assert!(cond.is_valid(&json!("a")) && cond.is_valid(&json!(1)));
    assert!(!cond.is_valid(&json!("")) && !cond.is_valid(&parse("1.5").unwrap()));
    let bare = schema(json!({ "then": false, "else": false }));
    assert!(bare.is_valid(&json!(1)), "then/else without if never apply");
}

#[test]
fn test_unique_items_compares_by_value() {
    let s = schema(json!({ "uniqueItems": true }));
    assert!(!s.is_valid(&parse(r#"[{"a": 1, "b": 2}, {"b": 2, "a": 1.0}]"#).unwrap()));
    assert!(s.is_valid(&json!([[1, 2], [2, 1], "1", 1, true, null])));
    assert!(!s.is_valid(&json!([false, false])));
}

#[test]
fn test_false_schema_and_types() {
    let s = schema(json!(false));
    assert_eq!(
        s.validate(&json!(null)).unwrap_err().errors()[0].message(),
        "no value is allowed here"
    );
    let s = schema(json!({ "type": ["null", "integer"] }));
    assert!(s.is_valid(&json!(null)) && s.is_valid(&json!(2)));
    assert_eq!(
        s.validate(&json!("x")).unwrap_err().errors()[0].message(),
        "expected null or integer, found string"
    );
}

#[test]
fn test_deep_instance_is_refused_within_budget() {
    let s = schema(json!({ "items": { "$ref": "#" } }));
    let mut deep = json!([]);
    for _ in 0..300 {
        deep = Value::Array(vec![deep].into());
    }
    assert!(s.validate(&deep).unwrap_err().budget_exceeded());
    let mut deep = json!(1);
    for _ in 0..600 {
        deep = Value::Array(vec![deep].into());
    }
    assert!(
        schema(json!({ "const": 1 }))
            .validate(&deep)
            .unwrap_err()
            .budget_exceeded()
    );
}

#[test]
fn test_limits_builders_and_accessors() {
    let limits = SchemaLimits::default()
        .with_max_errors(2)
        .with_max_validation_depth(3);
    let s = Schema::compile_with(&json!(true), limits).unwrap();
    assert_eq!(s.limits(), limits);
    assert_eq!(
        SchemaError::new("/x".into(), SchemaErrorKind::TooDeep { limit: 1 }).to_string(),
        "invalid schema at /x: subschemas nest deeper than the limit of 1"
    );
    assert_eq!(
        refused(json!(null)).to_string(),
        "invalid schema at /: a schema must be an object or a boolean, found null"
    );
}

#[test]
fn test_every_error_kind_displays_its_fix() {
    let kinds = [
        SchemaErrorKind::NotASchema { found: "string" },
        SchemaErrorKind::InvalidKeyword {
            keyword: "minimum".into(),
            reason: "must be a number",
        },
        SchemaErrorKind::Unsupported {
            keyword: "pattern".into(),
        },
        SchemaErrorKind::UnsupportedDialect {
            dialect: "draft-07".into(),
        },
        SchemaErrorKind::DuplicateAnchor { name: "a".into() },
        SchemaErrorKind::NonLocalRef {
            reference: "x.json".into(),
        },
        SchemaErrorKind::UnresolvedRef {
            reference: "#/x".into(),
        },
        SchemaErrorKind::TooDeep { limit: 2 },
        SchemaErrorKind::TooLarge { limit: 3 },
    ];
    let texts: Vec<String> = kinds.iter().map(ToString::to_string).collect();
    assert_eq!(texts[1], "`minimum` must be a number");
    assert_eq!(texts[2], "`pattern` is not supported by this validator");
    assert!(texts[5].contains("never fetched"));
    assert!(texts.iter().all(|t| !t.is_empty()));
    let error = SchemaError::new(String::new(), SchemaErrorKind::TooLarge { limit: 3 });
    assert_eq!(
        (error.pointer(), error.kind()),
        ("", &SchemaErrorKind::TooLarge { limit: 3 })
    );
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn test_limit_builders_set_each_field() {
    let limits = SchemaLimits::new()
        .with_max_depth(5)
        .with_max_nodes(6)
        .with_max_steps(7)
        .with_max_validation_depth(8)
        .with_max_errors(9);
    assert_eq!(
        (
            limits.max_depth,
            limits.max_nodes,
            limits.max_steps,
            limits.max_validation_depth,
            limits.max_errors
        ),
        (5, 6, 7, 8, 9)
    );
    let violations = schema(json!({ "type": "string" }))
        .validate(&json!(1))
        .unwrap_err();
    assert!(std::error::Error::source(&violations).is_none());
    assert_eq!(
        violations.errors()[0].to_string(),
        "(root): expected string, found number"
    );
}

#[test]
fn test_validation_depth_limit_applies() {
    let s = Schema::compile_with(
        &json!({ "items": { "items": { "items": {} } } }),
        SchemaLimits::new().with_max_validation_depth(1),
    )
    .unwrap();
    assert!(s.validate(&json!([[[1]]])).unwrap_err().budget_exceeded());
    assert!(s.is_valid(&json!([])));
}

#[test]
fn test_validation_at_full_depth_fits_a_one_mebibyte_stack() {
    // 1 MiB is a Windows main thread's stack, where a server with no worker
    // threads validates. A recursive schema over 255 levels of arrays costs
    // 511 nested applications, just inside the default cap of 512.
    let worker = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(|| {
            let s = schema(
                json!({ "type": "array", "items": { "$ref": "#" }, "anyOf": [{ "maxItems": 1 }] }),
            );
            let mut deep = json!([]);
            for _ in 0..255 {
                deep = Value::Array(vec![deep].into());
            }
            let fits = s.validate(&deep).is_ok();
            let deeper = Value::Array(vec![Value::Array(vec![deep].into())].into());
            (
                fits,
                s.validate(&deeper).is_err_and(|v| v.budget_exceeded()),
            )
        })
        .unwrap();
    assert_eq!(worker.join().unwrap(), (true, true));
}

#[test]
fn test_numbers_beyond_the_exact_range_are_refused_not_misjudged() {
    let unique = schema(json!({ "uniqueItems": true }));
    let huge = parse("[1e4503599627370497, 1e4503599627370498]").unwrap();
    let errors = unique.validate(&huge).unwrap_err();
    assert_eq!(errors.errors()[0].keyword(), "number");
    let bound = schema(json!({ "maximum": 10 }));
    assert_eq!(
        bound
            .validate(&parse("1e9999999999999999999").unwrap())
            .unwrap_err()
            .errors()[0]
            .keyword(),
        "number"
    );
    let integer = schema(json!({ "type": "integer" }));
    assert!(
        integer
            .validate(&parse("1e9999999999999999999").unwrap())
            .is_err()
    );
    // Without a keyword that needs its value, such a number is still a number.
    assert!(schema(json!({ "type": "number" })).is_valid(&parse("1e9999999999999999999").unwrap()));
    assert!(schema(json!({ "type": "array" })).is_valid(&huge));
}

#[test]
fn test_canonical_text_is_charged_to_the_step_budget() {
    // `enum` at every level of a deep array re-reads the whole subtree: the
    // charge per byte makes that refuse instead of taking quadratic time.
    let s = Schema::compile_with(
        &json!({ "items": { "$ref": "#" }, "not": { "enum": [0] } }),
        SchemaLimits::new().with_max_steps(20_000),
    )
    .unwrap();
    let mut deep = Value::Array(
        (0..2000)
            .map(|i| Value::from(i64::from(i)))
            .collect::<Vec<_>>()
            .into(),
    );
    for _ in 0..100 {
        deep = Value::Array(vec![deep].into());
    }
    assert!(s.validate(&deep).unwrap_err().budget_exceeded());
}
