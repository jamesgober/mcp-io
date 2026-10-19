//! The official JSON Schema Test Suite (draft 2020-12), run against
//! `mcp_io::Schema`.
//!
//! The data lives in `tests/data/json-schema-test-suite/` (MIT, see the
//! `LICENSE` and `NOTICE` there). Every case must either pass, or belong to a
//! schema the compiler refuses with an "unsupported" kind of error: the
//! validator may decline a schema, but it may never give a wrong answer for
//! one it accepted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::{collections::BTreeMap, fs, path::Path};

use mcp_io::{Schema, SchemaErrorKind, jsonx};

/// Files whose every group uses a keyword or construct the validator refuses
/// at compile time (see the `schema` module documentation).
const REFUSED_FILES: &[&str] = &[
    "dynamicRef.json",
    "pattern.json",
    "patternProperties.json",
    "refRemote.json",
    "unevaluatedItems.json",
    "unevaluatedProperties.json",
    "vocabulary.json",
];

#[test]
fn test_json_schema_test_suite_draft2020_12_never_answers_wrong() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/json-schema-test-suite");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 46, "the vendored suite is incomplete");

    let mut wrong = Vec::new();
    let mut passed = 0usize;
    let mut refused: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(path).unwrap();
        let groups = jsonx::parse(&text).unwrap();
        for group in groups.as_array().unwrap().iter() {
            let description = group["description"].as_str().unwrap_or_default();
            let tests = group["tests"].as_array().unwrap();
            let schema = match Schema::compile(&group["schema"]) {
                Ok(schema) => schema,
                Err(error) => {
                    assert!(
                        matches!(
                            error.kind(),
                            SchemaErrorKind::Unsupported { .. }
                                | SchemaErrorKind::NonLocalRef { .. }
                                | SchemaErrorKind::UnsupportedDialect { .. }
                        ),
                        "{name}: {description}: refused for a reason other than lack of support: {error}",
                    );
                    *refused.entry(name.clone()).or_default() += tests.len();
                    continue;
                }
            };
            for test in tests.iter() {
                let expected = test["valid"].as_bool().unwrap();
                let actual = schema.is_valid(&test["data"]);
                if actual == expected {
                    passed += 1;
                } else {
                    wrong.push(format!(
                        "{name}: {description}: {}: expected {expected}",
                        test["description"].as_str().unwrap_or_default()
                    ));
                }
            }
        }
    }
    eprintln!(
        "JSON Schema Test Suite: {passed} cases passed; refused at compile time: {refused:?}"
    );
    assert!(
        wrong.is_empty(),
        "{} wrong answers:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    // 920 when this was written: fewer means a supported case regressed into
    // a refusal.
    assert!(passed >= 920, "only {passed} cases ran and passed");
    for file in REFUSED_FILES {
        assert!(
            refused.contains_key(*file),
            "{file} was expected to be refused"
        );
    }
}
