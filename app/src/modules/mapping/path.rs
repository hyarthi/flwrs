//! Dot paths: `a.b.c`, array indexes `a[0]`, quoted keys for unusual names
//! `a["content.type"]`. Bare keys are letters, digits, `_` and `-`.

use super::value::Value;
use flwrs_plugin::schema::schema::{
    FieldDefinition, SchemaDefinition, field_type::Enum as FieldType,
};
use std::fmt;

const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Path(pub(crate) Vec<Segment>);

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(k) if is_bare(k) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(k)?;
                }
                Segment::Key(k) => write!(f, "[{k:?}]")?,
                Segment::Index(n) => write!(f, "[{n}]")?,
            }
        }
        Ok(())
    }
}

fn is_bare(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Path {
    /// Parses a path. It must start with a field name (a payload is an object).
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let bytes = text.as_bytes();
        let mut segments = vec![];
        let mut i = 0;
        let mut after_dot = false;
        let err = |msg: &str| format!("path {text:?}: {msg}");
        while i < bytes.len() {
            match bytes[i] {
                b'.' => {
                    if segments.is_empty() || i + 1 >= bytes.len() {
                        return Err(err("a '.' must be between two keys"));
                    }
                    i += 1;
                    if bytes[i] == b'.' || bytes[i] == b'[' {
                        return Err(err("expected a key after '.'"));
                    }
                    after_dot = true;
                }
                b'[' => {
                    after_dot = false;
                    i += 1;
                    if bytes.get(i) == Some(&b'"') {
                        // Quoted key with \" and \\ escapes.
                        i += 1;
                        let mut key = String::new();
                        loop {
                            match bytes.get(i) {
                                None => return Err(err("unterminated quoted key")),
                                Some(b'"') => break,
                                Some(b'\\') => {
                                    match bytes.get(i + 1) {
                                        Some(b'"') => key.push('"'),
                                        Some(b'\\') => key.push('\\'),
                                        _ => return Err(err("only \\\" and \\\\ can be escaped")),
                                    }
                                    i += 2;
                                    continue;
                                }
                                Some(_) => {
                                    // Push the whole UTF-8 character.
                                    let ch = text[i..].chars().next().expect("in bounds");
                                    key.push(ch);
                                    i += ch.len_utf8();
                                    continue;
                                }
                            }
                        }
                        i += 1; // closing quote
                        if bytes.get(i) != Some(&b']') {
                            return Err(err("expected ']' after a quoted key"));
                        }
                        i += 1;
                        if key.is_empty() {
                            return Err(err("empty key"));
                        }
                        segments.push(Segment::Key(key));
                    } else {
                        let start = i;
                        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                            i += 1;
                        }
                        if start == i || bytes.get(i) != Some(&b']') {
                            return Err(err("expected an index like [0] or a key like [\"k\"]"));
                        }
                        let index = text[start..i]
                            .parse::<usize>()
                            .map_err(|_| err("index too large"))?;
                        i += 1;
                        if segments.is_empty() {
                            return Err(err("must start with a field name"));
                        }
                        segments.push(Segment::Index(index));
                    }
                    continue;
                }
                _ => {}
            }
            if !segments.is_empty() && !after_dot {
                return Err(err("expected '.' or '[' between segments"));
            }
            after_dot = false;
            let start = i;
            while bytes
                .get(i)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            {
                i += 1;
            }
            if start == i {
                return Err(err(&format!(
                    "unexpected {:?}",
                    text[i..].chars().next().unwrap_or(' ')
                )));
            }
            segments.push(Segment::Key(text[start..i].to_string()));
        }
        if segments.is_empty() {
            return Err(err("empty"));
        }
        if segments.len() > MAX_DEPTH {
            return Err(err(&format!("deeper than {MAX_DEPTH} levels")));
        }
        Ok(Path(segments))
    }

    pub(crate) fn get<'a>(&self, value: &'a Value) -> Option<&'a Value> {
        let mut current = value;
        for segment in &self.0 {
            current = match (segment, current) {
                (Segment::Key(k), Value::Object(fields)) => {
                    &fields.iter().find(|(key, _)| key == k)?.1
                }
                (Segment::Key(k), Value::Map(map)) => map.get(k)?,
                (Segment::Index(n), Value::Array(items)) => items.get(*n)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Sets the value at the path, creating objects along the way (an index
    /// may append: `a[len]`).
    pub(crate) fn set(&self, root: &mut Value, new: Value) -> Result<(), String> {
        let mut current = root;
        for (depth, segment) in self.0.iter().enumerate() {
            let last = depth + 1 == self.0.len();
            if *current == Value::Null {
                *current = match segment {
                    Segment::Key(_) => Value::Object(vec![]),
                    Segment::Index(_) => Value::Array(vec![]),
                };
            }
            let slot: &mut Value = match (segment, current) {
                (Segment::Key(k), Value::Object(fields)) => {
                    let at = match fields.iter().position(|(key, _)| key == k) {
                        Some(at) => at,
                        None => {
                            fields.push((k.clone(), Value::Null));
                            fields.len() - 1
                        }
                    };
                    &mut fields[at].1
                }
                (Segment::Key(k), Value::Map(map)) => map.entry(k.clone()).or_insert(Value::Null),
                (Segment::Index(n), Value::Array(items)) => {
                    if *n == items.len() {
                        items.push(Value::Null);
                    }
                    let len = items.len();
                    items
                        .get_mut(*n)
                        .ok_or_else(|| format!("{self}: index {n} is past the end ({len} items)"))?
                }
                (segment, other) => {
                    return Err(format!(
                        "{self}: can't use {} on a {}",
                        match segment {
                            Segment::Key(k) => format!("key {k:?}"),
                            Segment::Index(n) => format!("index {n}"),
                        },
                        other.type_name()
                    ));
                }
            };
            if last {
                *slot = new;
                return Ok(());
            }
            current = slot;
        }
        unreachable!("paths aren't empty")
    }

    /// Removes the value at the path. Returns whether there was one.
    pub(crate) fn delete(&self, root: &mut Value) -> bool {
        let (last, parents) = self.0.split_last().expect("paths aren't empty");
        let mut current = root;
        for segment in parents {
            current = match (segment, current) {
                (Segment::Key(k), Value::Object(fields)) => {
                    match fields.iter_mut().find(|(key, _)| key == k) {
                        Some((_, v)) => v,
                        None => return false,
                    }
                }
                (Segment::Key(k), Value::Map(map)) => match map.get_mut(k) {
                    Some(v) => v,
                    None => return false,
                },
                (Segment::Index(n), Value::Array(items)) => match items.get_mut(*n) {
                    Some(v) => v,
                    None => return false,
                },
                _ => return false,
            };
        }
        match (last, current) {
            (Segment::Key(k), Value::Object(fields)) => {
                let before = fields.len();
                fields.retain(|(key, _)| key != k);
                fields.len() != before
            }
            (Segment::Key(k), Value::Map(map)) => map.remove(k).is_some(),
            (Segment::Index(n), Value::Array(items)) if *n < items.len() => {
                items.remove(*n);
                true
            }
            _ => false,
        }
    }

    /// The field definition the path points at in `schema`: keys of OBJECTs
    /// must be declared, any key of a MAP works, indexes need an ARRAY.
    pub(crate) fn resolve<'a>(
        &self,
        schema: &'a SchemaDefinition,
    ) -> Result<&'a FieldDefinition, String> {
        let mut fields: &'a [FieldDefinition] = &schema.fields;
        let mut current: Option<&'a FieldDefinition> = None;
        for segment in &self.0 {
            let next = match (segment, current) {
                (Segment::Key(k), None) => fields.iter().find(|f| f.key == *k),
                (Segment::Key(k), Some(def)) => match def.r#type() {
                    FieldType::Object => {
                        fields = &def.object_fields;
                        fields.iter().find(|f| f.key == *k)
                    }
                    FieldType::Map => def.nested_type_definition.as_deref(),
                    other => {
                        return Err(format!(
                            "{self}: {} is a {}, so it has no key {k:?}",
                            def.key,
                            other.as_str_name()
                        ));
                    }
                },
                (Segment::Index(n), Some(def)) => match def.r#type() {
                    FieldType::Array => def.nested_type_definition.as_deref(),
                    other => {
                        return Err(format!(
                            "{self}: {} is a {}, not an ARRAY, so it has no index {n}",
                            def.key,
                            other.as_str_name()
                        ));
                    }
                },
                (Segment::Index(_), None) => unreachable!("paths start with a key"),
            };
            current = Some(next.ok_or_else(|| match segment {
                Segment::Key(k) => format!("{self}: no field {k:?} in the schema"),
                Segment::Index(_) => format!("{self}: the ARRAY has no element type"),
            })?);
        }
        current.ok_or_else(|| format!("{self}: empty"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(fields: Vec<(&str, Value)>) -> Value {
        Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn parses_dots_indexes_and_quoted_keys() {
        let path = Path::parse(r#"headers["content.type"][0].x-y_1"#).unwrap();
        assert_eq!(
            path.0,
            [
                Segment::Key("headers".into()),
                Segment::Key("content.type".into()),
                Segment::Index(0),
                Segment::Key("x-y_1".into()),
            ]
        );
        assert_eq!(path.to_string(), r#"headers["content.type"][0].x-y_1"#);
        assert_eq!(
            Path::parse(r#"["a \"b\""]"#).unwrap().0,
            [Segment::Key("a \"b\"".into())]
        );
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "a[",
            "a[x]",
            "[0]",
            "a[\"\"]",
            "a b",
            "a[\"x\"",
            "a.[0]",
            "a[0]x",
            "a[\"k\"]b",
        ] {
            assert!(Path::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn get_set_delete() {
        let mut v = obj(vec![
            ("a", obj(vec![("b", Value::I32(1))])),
            ("list", Value::Array(vec![Value::I32(7)])),
        ]);
        assert_eq!(Path::parse("a.b").unwrap().get(&v), Some(&Value::I32(1)));
        assert_eq!(
            Path::parse("list[0]").unwrap().get(&v),
            Some(&Value::I32(7))
        );
        assert_eq!(Path::parse("list[1]").unwrap().get(&v), None);

        Path::parse("x.y.z")
            .unwrap()
            .set(&mut v, Value::Bool(true))
            .unwrap();
        assert_eq!(
            Path::parse("x.y.z").unwrap().get(&v),
            Some(&Value::Bool(true))
        );
        Path::parse("list[1]")
            .unwrap()
            .set(&mut v, Value::I32(8))
            .unwrap();
        assert!(
            Path::parse("list[5]")
                .unwrap()
                .set(&mut v, Value::I32(9))
                .is_err()
        );
        assert!(
            Path::parse("a.b.c")
                .unwrap()
                .set(&mut v, Value::Null)
                .is_err(),
            "b is an I32"
        );

        assert!(Path::parse("a.b").unwrap().delete(&mut v));
        assert!(!Path::parse("a.b").unwrap().delete(&mut v));
        assert!(Path::parse("list[0]").unwrap().delete(&mut v));
        assert_eq!(
            Path::parse("list[0]").unwrap().get(&v),
            Some(&Value::I32(8))
        );
    }

    #[test]
    fn resolves_against_a_schema() {
        let schema: SchemaDefinition = flwrs_plugin::schema::rules::parse_json(
            r#"{"fields": [
                {"key": "url", "type": "STRING"},
                {"key": "headers", "type": "MAP", "nested_type_definition": {"type": "ARRAY", "nested_type_definition": {"type": "STRING"}}},
                {"key": "point", "type": "OBJECT", "object_fields": [{"key": "x", "type": "F64"}]}
            ]}"#,
        )
        .unwrap();
        let ty = |p: &str| Path::parse(p).unwrap().resolve(&schema).map(|d| d.r#type());
        assert_eq!(ty("url"), Ok(FieldType::String));
        assert_eq!(ty(r#"headers["Content-Type"][0]"#), Ok(FieldType::String));
        assert_eq!(ty("point.x"), Ok(FieldType::F64));
        assert!(ty("nope").unwrap_err().contains("no field \"nope\""));
        assert!(ty("point.y").is_err());
        assert!(ty("url.x").unwrap_err().contains("is a STRING"));
        assert!(ty("url[0]").unwrap_err().contains("not an ARRAY"));
    }
}
