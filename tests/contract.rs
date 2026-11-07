//! `/api/v1` only grows (ADR 0005 of Ruchoir/.github): nothing a client built against the recorded
//! contract relies on may go, and what was added is recorded in the same change.
//!
//! `RUCHOIR_UPDATE_CONTRACT=1 cargo test --test contract` writes `contract/openapi-v1.json`, and
//! refuses to while something breaks.

use std::collections::BTreeMap;

use hub::openapi::ApiDoc;
use ruchoir_contract::breaking_changes;
use serde_json::Value;
use utoipa::OpenApi;

const RECORDED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/contract/openapi-v1.json");

/// Objects with their keys in order, as the recorded file keeps them.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let map: BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            Value::Object(map.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// The release number is not part of the contract: a new version changes it and breaks nothing.
fn without_release(mut doc: Value) -> Value {
    if let Some(info) = doc.get_mut("info").and_then(Value::as_object_mut) {
        info.remove("version");
    }
    doc
}

#[test]
fn the_v1_contract_only_grows() {
    let current = sorted(serde_json::to_value(ApiDoc::openapi()).expect("the contract as JSON"));
    let recorded: Value = match std::fs::read_to_string(RECORDED) {
        Ok(text) => sorted(serde_json::from_str(&text).expect("the recorded contract is JSON")),
        Err(_) if std::env::var_os("RUCHOIR_UPDATE_CONTRACT").is_some() => current.clone(),
        Err(error) => panic!("cannot read {RECORDED}: {error}"),
    };

    let broken = breaking_changes(&recorded, &current);
    assert!(
        broken.is_empty(),
        "/api/v1 must only grow; this breaks clients that were not updated:\n{}",
        broken.join("\n")
    );

    if std::env::var_os("RUCHOIR_UPDATE_CONTRACT").is_some() {
        let mut text = serde_json::to_string_pretty(&without_release(current)).expect("JSON");
        text.push('\n');
        std::fs::write(RECORDED, text).expect("write the contract");
        return;
    }
    assert!(
        without_release(recorded) == without_release(current),
        "the API grew: record it with `RUCHOIR_UPDATE_CONTRACT=1 cargo test --test contract`"
    );
}
