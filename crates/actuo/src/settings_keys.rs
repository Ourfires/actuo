//! `actuo settings keys`: every key of `settings.toml` with its default and what
//! it does, read from the settings types' schema (their doc comments and serde
//! defaults, `actuo_protocol::settings`), so it can't fall behind them.

use actuo_protocol::settings::Settings;
use serde_json::Value;

pub fn keys() -> String {
    let root = serde_json::to_value(schemars::schema_for!(Settings)).expect("a schema");
    let mut out = String::new();
    table(&root, &root, "", &mut out);
    out
}

/// The keys of the table `schema`, under `prefix`.
fn table(root: &Value, schema: &Value, prefix: &str, out: &mut String) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        let key = format!("{prefix}{name}");
        let target = resolve(root, property);
        if target.get("properties").is_some() {
            table(root, target, &format!("{key}."), out);
            continue;
        }
        // A table of tables by name: only `projects`, by `owner/repo`.
        let entry = property
            .get("additionalProperties")
            .map(|v| resolve(root, v))
            .filter(|v| v.get("properties").is_some());
        if let Some(entry) = entry {
            let key = format!("{key}.\"owner/repo\"");
            out.push_str(&format!("{key}\n"));
            doc(property, out);
            table(root, entry, &format!("{key}."), out);
            continue;
        }
        let nullable = property["type"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t == "null"))
            || property.get("anyOf").is_some();
        let value = match (property.get("default"), property["type"].as_str()) {
            (Some(d), _) => format!(" = {}", toml_value(d)),
            (None, Some("array")) if !nullable => " = []".into(),
            (None, Some("object")) if !nullable => " = {}".into(),
            _ if prefix.starts_with("projects.") && lane_has(root, name) => {
                format!("  (unset: lane.{name} applies)")
            }
            _ => "  (unset)".into(),
        };
        out.push_str(&format!("{key}{value}\n"));
        doc(property, out);
    }
}

/// A property's schema, through its `$ref`, also inside an optional's `anyOf`.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let schema = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|v| v.get("$ref").is_some()))
        .unwrap_or(schema);
    match schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        Some(name) => &root["$defs"][name],
        None => schema,
    }
}

fn lane_has(root: &Value, name: &str) -> bool {
    root["$defs"]["LaneSettings"]["properties"]
        .get(name)
        .is_some()
}

/// The property's doc comment, indented under its key.
fn doc(property: &Value, out: &mut String) {
    if let Some(text) = property.get("description").and_then(Value::as_str) {
        for line in text.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
}

/// A default as TOML writes it.
fn toml_value(v: &Value) -> String {
    match v {
        Value::Object(m) if m.is_empty() => "{}".into(),
        v => toml::Value::try_from(v)
            .map(|t| t.to_string())
            .unwrap_or_else(|_| v.to_string()),
    }
}
