//! Dot paths into JSON messages (`id`, `meta.request_id`, `items[0].id`):
//! where the reply mode puts a request's id and finds a reply's.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
}

pub type JsonPath = Vec<Segment>;

pub fn parse(path: &str) -> Result<JsonPath, String> {
    let bad = |why: &str| format!("JSON path {path:?}: {why}");
    if path.is_empty() {
        return Err(bad("empty"));
    }
    let mut segments = vec![];
    for part in path.split('.') {
        let (key, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if key.is_empty() && (segments.is_empty() || rest.is_empty()) {
            return Err(bad("an empty key"));
        }
        if !key.is_empty() {
            segments.push(Segment::Key(key.to_string()));
        }
        while !rest.is_empty() {
            let close = rest.find(']').ok_or_else(|| bad("an unclosed ["))?;
            let index = rest[1..close]
                .parse::<usize>()
                .map_err(|_| bad("an index must be a number"))?;
            segments.push(Segment::Index(index));
            rest = &rest[close + 1..];
            if !rest.is_empty() && !rest.starts_with('[') {
                return Err(bad("unexpected text after ]"));
            }
        }
    }
    Ok(segments)
}

pub fn get<'a>(value: &'a Value, path: &[Segment]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, segment| match segment {
        Segment::Key(k) => v.get(k),
        Segment::Index(i) => v.get(i),
    })
}

/// Sets the value at `path`, creating objects for missing keys (an index
/// must already exist).
pub fn set(value: &mut Value, path: &[Segment], new: Value) -> Result<(), String> {
    let Some((last, parents)) = path.split_last() else {
        return Err("an empty path".into());
    };
    let mut at = value;
    for segment in parents {
        at = match segment {
            Segment::Key(k) => {
                let object = at
                    .as_object_mut()
                    .ok_or_else(|| format!("{k:?}: not inside an object"))?;
                object
                    .entry(k.clone())
                    .or_insert_with(|| Value::Object(Default::default()))
            }
            Segment::Index(i) => at.get_mut(i).ok_or_else(|| format!("no index {i}"))?,
        };
    }
    match last {
        Segment::Key(k) => {
            at.as_object_mut()
                .ok_or_else(|| format!("{k:?}: not inside an object"))?
                .insert(k.clone(), new);
        }
        Segment::Index(i) => *at.get_mut(i).ok_or_else(|| format!("no index {i}"))? = new,
    }
    Ok(())
}

/// A JSON value as an id: strings as they are, numbers as written.
pub fn id_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_gets_and_sets() {
        let p = parse("a.b[1].c").unwrap();
        assert_eq!(
            p,
            [
                Segment::Key("a".into()),
                Segment::Key("b".into()),
                Segment::Index(1),
                Segment::Key("c".into())
            ]
        );
        let v = json!({"a": {"b": [0, {"c": 7}]}});
        assert_eq!(get(&v, &p), Some(&json!(7)));
        let mut v = json!({"x": 1});
        set(&mut v, &parse("meta.id").unwrap(), json!("r1")).unwrap();
        assert_eq!(v, json!({"x": 1, "meta": {"id": "r1"}}));
        assert!(set(&mut json!([1]), &parse("id").unwrap(), json!(1)).is_err());
        for bad in ["", "a..b", "a[", "a[x]", "a[0]b", "[0]"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(id_of(&json!(12)).as_deref(), Some("12"));
        assert_eq!(id_of(&json!(null)), None);
    }
}
