//! The config file as text the page edits, with `toml_edit` so comments and
//! layout stay as written: secrets replaced by a placeholder for the browser
//! and put back on save, and a form's values written into a section.

use super::catalog::{Section, child_keys, sections};
use serde_json::{Map, Value as Json};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// What the page sees instead of a secret's value; sent back unchanged, it
/// keeps the value in the file.
pub(crate) const SECRET: &str = "(secret: kept)";

pub(crate) fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| e.to_string())
}

/// Every existing secret's path in `doc` (wildcards expanded).
fn secret_paths(doc: &DocumentMut) -> Vec<Vec<String>> {
    let mut out = vec![];
    for section in sections() {
        for pointer in section.secrets {
            let mut pattern: Vec<String> = section.path().iter().map(|s| s.to_string()).collect();
            pattern.extend(
                pointer
                    .trim_start_matches('/')
                    .split('/')
                    .map(str::to_string),
            );
            expand(doc.as_item(), &pattern, vec![], &mut out);
        }
    }
    out
}

fn expand(item: &Item, pattern: &[String], at: Vec<String>, out: &mut Vec<Vec<String>>) {
    let Some((first, rest)) = pattern.split_first() else {
        if item.is_value() {
            out.push(at);
        }
        return;
    };
    let Some(table) = item.as_table_like() else {
        return;
    };
    if first == "*" {
        for (key, child) in table.iter() {
            let mut next = at.clone();
            next.push(key.to_string());
            expand(child, rest, next, out);
        }
    } else if let Some(child) = table.get(first) {
        let mut next = at;
        next.push(first.clone());
        expand(child, rest, next, out);
    }
}

fn item_at<'a>(item: &'a Item, path: &[String]) -> Option<&'a Item> {
    path.iter()
        .try_fold(item, |item, key| item.as_table_like()?.get(key))
}

fn item_at_mut<'a>(item: &'a mut Item, path: &[String]) -> Option<&'a mut Item> {
    path.iter()
        .try_fold(item, |item, key| item.as_table_like_mut()?.get_mut(key))
}

/// Replaces a value, keeping the comments and spacing around it.
fn set_value(item: &mut Item, mut value: Value) {
    if let Some(old) = item.as_value() {
        *value.decor_mut() = old.decor().clone();
    }
    *item = Item::Value(value);
}

/// The text with every secret replaced by [`SECRET`]. Refused if the text
/// doesn't parse (its secrets can't be found then).
pub(crate) fn redact(text: &str) -> Result<String, String> {
    let mut doc = parse(text)?;
    for path in secret_paths(&doc) {
        if let Some(item) = item_at_mut(doc.as_item_mut(), &path) {
            set_value(item, Value::from(SECRET));
        }
    }
    Ok(doc.to_string())
}

/// `new` with each [`SECRET`] placeholder replaced by the value in
/// `current` (the file on disk). A placeholder for a secret the file doesn't
/// have is a problem.
pub(crate) fn restore_secrets(new: &str, current: &str) -> Result<String, Vec<String>> {
    let mut doc = parse(new).map_err(|e| vec![e])?;
    let old = parse(current).ok();
    let mut problems = vec![];
    for path in secret_paths(&doc) {
        let item = item_at_mut(doc.as_item_mut(), &path).expect("found above");
        if item.as_str() != Some(SECRET) {
            continue;
        }
        match old
            .as_ref()
            .and_then(|o| item_at(o.as_item(), &path))
            .and_then(Item::as_value)
        {
            Some(value) => set_value(item, value.clone()),
            None => problems.push(format!(
                "{}: the placeholder {SECRET:?} stands for a value the file doesn't have; type the secret",
                path.join(".")
            )),
        }
    }
    if problems.is_empty() {
        Ok(doc.to_string())
    } else {
        Err(problems)
    }
}

/// `text` with section `key` set to `values` (a form's object): keys it
/// leaves out (or sets to null) are removed, so their defaults apply; equal
/// values stay untouched, with their comments; [`SECRET`] keeps a secret.
pub(crate) fn apply_form(text: &str, key: &str, values: &Json) -> Result<String, String> {
    let section = sections()
        .iter()
        .find(|s| s.key == key)
        .ok_or_else(|| format!("no section [{key}]"))?;
    let values = values.as_object().ok_or("the values must be an object")?;
    let mut doc = parse(text)?;
    let path = section.path();
    let mut created = false;
    let mut item = doc.as_item_mut();
    for (i, part) in path.iter().enumerate() {
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| format!("[{}] isn't a table", path[..i].join(".")))?;
        if table.get(part).is_none() {
            let mut new = Table::new();
            new.set_implicit(i + 1 < path.len());
            table.insert(part, Item::Table(new));
            created = true;
        }
        item = table.get_mut(part).expect("inserted");
    }
    let keep = child_keys(section);
    let schema = (section.schema)();
    merge(
        item,
        values,
        &keep,
        Shape {
            root: &schema,
            schema: Some(&schema),
        },
    )?;
    let empty = item.as_table_like().is_some_and(|t| t.is_empty());
    if created && empty {
        remove_path(&mut doc, &path);
    }
    Ok(doc.to_string())
}

fn remove_path(doc: &mut DocumentMut, path: &[&str]) {
    let (last, parents) = path.split_last().expect("not empty");
    let parents: Vec<String> = parents.iter().map(|s| s.to_string()).collect();
    if let Some(table) = item_at_mut(doc.as_item_mut(), &parents).and_then(Item::as_table_like_mut)
    {
        table.remove(last);
    }
}

/// Where a value sits in the section's schema: new tables get their keys in
/// the declared order (`x-order`), the rest after them.
#[derive(Clone, Copy)]
struct Shape<'a> {
    root: &'a Json,
    schema: Option<&'a Json>,
}

impl<'a> Shape<'a> {
    fn resolved(self) -> Option<&'a Json> {
        let schema = self.schema?;
        match schema.get("$ref").and_then(Json::as_str) {
            Some(r) => r
                .strip_prefix('#')
                .and_then(|p| self.root.pointer(p))
                .or(Some(schema)),
            None => Some(schema),
        }
    }

    fn property(self, key: &str) -> Shape<'a> {
        Shape {
            root: self.root,
            schema: self.resolved().and_then(|s| s.get("properties")?.get(key)),
        }
    }

    fn items(self) -> Shape<'a> {
        Shape {
            root: self.root,
            schema: self.resolved().and_then(|s| s.get("items")),
        }
    }

    /// `map`'s keys: the declared ones first, in order, then the rest.
    fn ordered(self, map: &Map<String, Json>) -> Vec<&String> {
        let declared: Vec<&str> = self
            .resolved()
            .and_then(|s| s.get("x-order")?.as_array())
            .map(|a| a.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default();
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_by_key(|k| declared.iter().position(|d| d == k).unwrap_or(usize::MAX));
        keys
    }
}

fn merge(
    item: &mut Item,
    values: &Map<String, Json>,
    keep: &[&str],
    shape: Shape,
) -> Result<(), String> {
    let table = item.as_table_like_mut().ok_or("not a table")?;
    let stale: Vec<String> = table
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !keep.contains(&k.as_str()) && values.get(k).is_none_or(Json::is_null))
        .collect();
    for key in stale {
        table.remove(&key);
    }
    for key in shape.ordered(values) {
        let value = &values[key];
        if value.is_null() || value.as_str() == Some(SECRET) {
            continue;
        }
        let child = shape.property(key);
        match table.get_mut(key) {
            Some(existing) if to_json(existing).as_ref() == Some(value) => {}
            Some(existing) if value.is_object() && existing.is_table_like() => {
                merge(existing, value.as_object().expect("object"), &[], child)?;
            }
            Some(existing) => {
                let new = to_item(
                    value,
                    existing.is_table() || existing.is_array_of_tables(),
                    child,
                )?;
                match new {
                    Item::Value(v) => set_value(existing, v),
                    other => *existing = other,
                }
            }
            None => {
                table.insert(key, to_item(value, true, child)?);
            }
        }
    }
    Ok(())
}

/// A JSON value as TOML: objects as tables (`[a.b]`) and arrays of objects
/// as arrays of tables where `tables`, inline otherwise.
fn to_item(value: &Json, tables: bool, shape: Shape) -> Result<Item, String> {
    Ok(match value {
        Json::Object(map) if tables => {
            let mut table = Table::new();
            for k in shape.ordered(map) {
                let v = &map[k];
                if !v.is_null() {
                    table.insert(k, to_item(v, true, shape.property(k))?);
                }
            }
            Item::Table(table)
        }
        Json::Array(items) if tables && !items.is_empty() && items.iter().all(Json::is_object) => {
            let mut array = ArrayOfTables::new();
            for v in items {
                if let Item::Table(t) = to_item(v, true, shape.items())? {
                    array.push(t);
                }
            }
            Item::ArrayOfTables(array)
        }
        other => Item::Value(to_value(other)?),
    })
}

fn to_value(value: &Json) -> Result<Value, String> {
    Ok(match value {
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::from(i),
            (None, Some(f)) => Value::from(f),
            _ => return Err(format!("{n} doesn't fit in a TOML number")),
        },
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(items) => {
            let mut array = Array::new();
            for v in items {
                array.push(to_value(v)?);
            }
            Value::Array(array)
        }
        Json::Object(map) => {
            let mut table = InlineTable::new();
            for (k, v) in map {
                if !v.is_null() {
                    table.insert(k, to_value(v)?);
                }
            }
            Value::InlineTable(table)
        }
        Json::Null => return Err("null can't be written to TOML".into()),
    })
}

/// An item as JSON (to compare with a form's value).
pub(crate) fn to_json(item: &Item) -> Option<Json> {
    match item {
        Item::None => None,
        Item::Value(v) => Some(value_json(v)),
        Item::Table(t) => Some(Json::Object(
            t.iter()
                .filter_map(|(k, v)| Some((k.to_string(), to_json(v)?)))
                .collect(),
        )),
        Item::ArrayOfTables(a) => Some(Json::Array(
            a.iter()
                .map(|t| to_json(&Item::Table(t.clone())).unwrap_or(Json::Null))
                .collect(),
        )),
    }
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::String(s) => Json::String(s.value().clone()),
        Value::Integer(i) => Json::from(*i.value()),
        Value::Float(f) => {
            serde_json::Number::from_f64(*f.value()).map_or(Json::Null, Json::Number)
        }
        Value::Boolean(b) => Json::Bool(*b.value()),
        Value::Datetime(d) => Json::String(d.value().to_string()),
        Value::Array(a) => Json::Array(a.iter().map(value_json).collect()),
        Value::InlineTable(t) => Json::Object(
            t.iter()
                .map(|(k, v)| (k.to_string(), value_json(v)))
                .collect(),
        ),
    }
}

/// A section's keys as the file sets them (JSON; its child sections left
/// out, secrets as [`SECRET`]).
pub(crate) fn file_values(doc: &DocumentMut, section: &Section) -> Json {
    let path: Vec<String> = section.path().iter().map(|s| s.to_string()).collect();
    let mut values = item_at(doc.as_item(), &path)
        .and_then(to_json)
        .unwrap_or_else(|| Json::Object(Map::new()));
    if let Json::Object(map) = &mut values {
        for child in child_keys(section) {
            map.remove(child);
        }
    }
    redact_json(&mut values, section);
    values
}

/// Secrets in a section's JSON values replaced by [`SECRET`] (where set).
pub(crate) fn redact_json(values: &mut Json, section: &Section) {
    for pointer in section.secrets {
        let parts: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
        redact_parts(values, &parts);
    }
}

fn redact_parts(value: &mut Json, parts: &[&str]) {
    let Some((first, rest)) = parts.split_first() else {
        if !value.is_null() {
            *value = Json::String(SECRET.into());
        }
        return;
    };
    let Json::Object(map) = value else {
        return;
    };
    if *first == "*" {
        for v in map.values_mut() {
            redact_parts(v, rest);
        }
    } else if let Some(v) = map.get_mut(*first) {
        redact_parts(v, rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FILE: &str = r#"# The dev config
[http]
host = "127.0.0.1" # loopback only
port = 8080

[http.auth]
secret_key = "a-very-long-secret-key"

[plugins]
dir = "./plugins"
admin_token = "token-1"

[telemetry.otlp]
headers = { authorization = "Bearer x", other = "y" }
"#;

    #[test]
    fn secrets_are_hidden_and_come_back() {
        let shown = redact(FILE).unwrap();
        assert!(!shown.contains("a-very-long-secret-key"));
        assert!(!shown.contains("token-1"));
        assert!(!shown.contains("Bearer x"));
        assert!(shown.contains("# loopback only"), "comments kept");
        assert_eq!(shown.matches(SECRET).count(), 4);
        // Sent back as it was: the file again.
        assert_eq!(restore_secrets(&shown, FILE).unwrap(), FILE);
        // A placeholder for a secret the file doesn't have.
        let problems = restore_secrets(
            "[plugins]\nadmin_token = \"(secret: kept)\"\n",
            "[plugins]\n",
        )
        .unwrap_err();
        assert!(
            problems[0].starts_with("plugins.admin_token"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_form_edits_one_section_and_keeps_the_rest() {
        let out = apply_form(
            FILE,
            "http",
            &json!({"host": "127.0.0.1", "port": 9090, "swagger_ui": true}),
        )
        .unwrap();
        assert!(
            out.contains("host = \"127.0.0.1\" # loopback only"),
            "{out}"
        );
        assert!(out.contains("port = 9090"));
        assert!(out.contains("swagger_ui = true"));
        assert!(
            out.contains("[http.auth]\nsecret_key = \"a-very-long-secret-key\""),
            "the child section stays: {out}"
        );
        // Left out = removed (its default applies); secrets kept by the placeholder.
        let out = apply_form(
            FILE,
            "plugins",
            &json!({"admin_token": SECRET, "max_entries": 5}),
        )
        .unwrap();
        assert!(!out.contains("dir = "), "{out}");
        assert!(
            out.contains("admin_token = \"token-1\"") && out.contains("max_entries = 5"),
            "{out}"
        );
        // A new section, nested values as tables.
        let out = apply_form(
            FILE,
            "hub",
            &json!({"port": 7400, "tls": {"port": 7401, "cert": "c.pem", "key": "k.pem"}}),
        )
        .unwrap();
        assert!(out.contains("[hub]\nport = 7400"), "{out}");
        assert!(out.contains("[hub.tls]"), "{out}");
        // Arrays of objects as arrays of tables.
        let out = apply_form(
            "",
            "logging",
            &json!({"level": "DEBUG", "sinks": [{"sink_type": "CONSOLE"}]}),
        )
        .unwrap();
        assert!(
            out.contains("[[logging.sinks]]\nsink_type = \"CONSOLE\""),
            "{out}"
        );
        // New tables in the declared order, not alphabetically.
        let out = apply_form(
            "",
            "logging",
            &json!({"sinks": [{"file_directory": "logs", "sink_type": "FILE"}]}),
        )
        .unwrap();
        assert!(
            out.contains("sink_type = \"FILE\"\nfile_directory = \"logs\""),
            "{out}"
        );
        // Nothing set in a new section: no empty header.
        assert_eq!(apply_form(FILE, "scenes", &json!({})).unwrap(), FILE);
        let parsed: toml::Table =
            toml::from_str(&apply_form(FILE, "db.main", &json!({"filename": "x.db"})).unwrap())
                .unwrap();
        assert_eq!(parsed["db"]["main"]["filename"].as_str(), Some("x.db"));
    }

    #[test]
    fn file_values_leave_out_children_and_hide_secrets() {
        let doc = parse(FILE).unwrap();
        let http = sections().iter().find(|s| s.key == "http").unwrap();
        assert_eq!(
            file_values(&doc, http),
            json!({"host": "127.0.0.1", "port": 8080})
        );
        let plugins = sections().iter().find(|s| s.key == "plugins").unwrap();
        assert_eq!(
            file_values(&doc, plugins),
            json!({"dir": "./plugins", "admin_token": SECRET})
        );
    }
}
