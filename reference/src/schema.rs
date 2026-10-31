//! Validation against the shared `jamlink.schema.json`.

use serde_json::Value;
use std::sync::OnceLock;

/// The schema, embedded from the repo's `template/` folder so there is one source of truth.
pub const SCHEMA_JSON: &str = include_str!("../../template/jamlink.schema.json");

fn validator() -> &'static jsonschema::Validator {
    static V: OnceLock<jsonschema::Validator> = OnceLock::new();
    V.get_or_init(|| {
        let schema: Value = serde_json::from_str(SCHEMA_JSON).expect("schema is valid JSON");
        jsonschema::validator_for(&schema).expect("schema compiles")
    })
}

/// Checks a message against the schema. On failure returns up to three
/// readable error descriptions, such as `/pos: [1,2] has less than 3 items`.
pub fn check(message: &Value) -> Result<(), Vec<String>> {
    let errors: Vec<String> = validator()
        .iter_errors(message)
        .take(3)
        .map(|e| {
            let path = e.instance_path().to_string();
            if path.is_empty() { e.to_string() } else { format!("{path}: {e}") }
        })
        .collect();
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
