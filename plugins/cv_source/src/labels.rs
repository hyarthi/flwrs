//! Class labels and CTC charsets: built-in lists, the node's files, and what
//! models carry in their metadata.

use crate::graph::config::{LabelSource, Labels, TextSource};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The built-in label lists, by name.
pub const BUILTIN: &[(&str, &str)] = &[
    ("coco80", include_str!("../labels/coco80.txt")),
    ("imagenet1k", include_str!("../labels/imagenet1k.txt")),
];

/// The most labels or charset entries read from one source.
const MAX_ENTRIES: usize = 100_000;

/// Where label and charset files come from (by name), and the model's
/// metadata (by key).
pub struct Sources<'a> {
    pub files: &'a BTreeMap<String, PathBuf>,
    pub metadata: &'a dyn Fn(&str) -> Option<String>,
}

impl Sources<'_> {
    fn file(&self, name: &str) -> Result<String, String> {
        let path = self
            .files
            .get(name)
            .ok_or_else(|| format!("the file {name:?} isn't among the node's files"))?;
        std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))
    }

    fn metadata(&self, key: &str) -> Result<String, String> {
        (self.metadata)(key).ok_or_else(|| format!("the model's metadata has no {key:?}"))
    }
}

/// A model's labels (none: class ids only).
pub fn labels(labels: Option<&Labels>, sources: &Sources) -> Result<Vec<String>, String> {
    let Some(labels) = labels else {
        return Ok(vec![]);
    };
    let list = match labels {
        Labels::List(list) => list.clone(),
        Labels::From(LabelSource::Builtin(name)) => BUILTIN
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, text)| lines(text))
            .ok_or_else(|| {
                format!("labels: no built-in list {name:?} (there are: coco80, imagenet1k)")
            })?,
        Labels::From(LabelSource::File(name)) => lines(&sources.file(name)?),
        Labels::From(LabelSource::ModelMetadata(key)) => python_dict(&sources.metadata(key)?)
            .map_err(|e| format!("labels: the model's metadata {key:?}: {e}"))?,
        Labels::From(LabelSource::HfConfig(name)) => {
            id2label(&sources.file(name)?).map_err(|e| format!("labels: {name}: {e}"))?
        }
    };
    if list.len() > MAX_ENTRIES {
        return Err(format!("labels: more than {MAX_ENTRIES}"));
    }
    Ok(list)
}

/// A CTC charset: one entry per line (index 0 of the model is the blank).
pub fn charset(source: Option<&TextSource>, sources: &Sources) -> Result<Vec<String>, String> {
    let text = match source {
        None => return Ok(vec![]),
        Some(TextSource::File(name)) => sources.file(name)?,
        Some(TextSource::ModelMetadata(key)) => sources.metadata(key)?,
    };
    let list = lines(&text);
    if list.len() > MAX_ENTRIES {
        return Err(format!("charset: more than {MAX_ENTRIES} entries"));
    }
    Ok(list)
}

/// One entry per line (`\n` or `\r\n`); a last empty line doesn't count.
fn lines(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return vec![];
    }
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect()
}

/// A list from the ids of `{id: label}` pairs; ids missing in between get
/// empty labels.
fn by_id(pairs: Vec<(usize, String)>) -> Result<Vec<String>, String> {
    let n = pairs.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
    if n > MAX_ENTRIES {
        return Err(format!("a class id of {} is too large", n - 1));
    }
    let mut list = vec![String::new(); n];
    for (i, label) in pairs {
        list[i] = label;
    }
    Ok(list)
}

/// Ultralytics' `names`: a Python dict literal, `{0: 'person', 1: "men's"}`.
fn python_dict(text: &str) -> Result<Vec<String>, String> {
    let body = text
        .trim()
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .ok_or("not a {id: 'name'} dict")?;
    let mut pairs = vec![];
    let mut rest = body.trim_start();
    while !rest.is_empty() {
        let colon = rest.find(':').ok_or("an entry without ':'")?;
        let id: usize = rest[..colon]
            .trim()
            .parse()
            .map_err(|_| format!("{:?} isn't a class id", rest[..colon].trim()))?;
        rest = rest[colon + 1..].trim_start();
        let quote = rest
            .chars()
            .next()
            .filter(|c| *c == '\'' || *c == '"')
            .ok_or("a name that isn't quoted")?;
        let mut name = String::new();
        let mut chars = rest[1..].char_indices();
        let end = loop {
            match chars.next() {
                None => return Err("an unterminated name".into()),
                Some((_, '\\')) => match chars.next() {
                    Some((_, c)) => name.push(c),
                    None => return Err("an unterminated name".into()),
                },
                Some((i, c)) if c == quote => break i + 2,
                Some((_, c)) => name.push(c),
            }
        };
        pairs.push((id, name));
        rest = rest[end..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    by_id(pairs)
}

/// A Hugging Face `config.json`'s `id2label`.
fn id2label(text: &str) -> Result<Vec<String>, String> {
    let config: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let map = config["id2label"].as_object().ok_or("no id2label object")?;
    let pairs = map
        .iter()
        .map(|(k, v)| {
            let id = k
                .parse()
                .map_err(|_| format!("id2label: {k:?} isn't a class id"))?;
            Ok((id, v.as_str().unwrap_or_default().to_string()))
        })
        .collect::<Result<_, String>>()?;
    by_id(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources<'a>(
        files: &'a BTreeMap<String, PathBuf>,
        metadata: &'a dyn Fn(&str) -> Option<String>,
    ) -> Sources<'a> {
        Sources { files, metadata }
    }

    #[test]
    fn built_in_lists() {
        let none = BTreeMap::new();
        let meta = |_: &str| None;
        let s = sources(&none, &meta);
        let coco = labels(
            Some(&Labels::From(LabelSource::Builtin("coco80".into()))),
            &s,
        )
        .unwrap();
        assert_eq!(
            (coco.len(), coco[0].as_str(), coco[79].as_str()),
            (80, "person", "toothbrush")
        );
        let imagenet = labels(
            Some(&Labels::From(LabelSource::Builtin("imagenet1k".into()))),
            &s,
        )
        .unwrap();
        assert_eq!(
            (imagenet.len(), imagenet[0].as_str(), imagenet[919].as_str()),
            (1000, "tench", "street sign")
        );
        assert!(labels(Some(&Labels::From(LabelSource::Builtin("voc".into()))), &s).is_err());
        assert!(labels(None, &s).unwrap().is_empty());
    }

    #[test]
    fn metadata_and_files() {
        let dir = std::env::temp_dir().join(format!("cv-labels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("l.txt"), "cat\r\ndog\n").unwrap();
        std::fs::write(
            dir.join("config.json"),
            r#"{"id2label": {"1": "dog", "0": "cat", "3": "fox"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("keys.txt"), "a\nb\n \n").unwrap();
        let files: BTreeMap<String, PathBuf> = ["l.txt", "config.json", "keys.txt"]
            .map(|n| (n.to_string(), dir.join(n)))
            .into_iter()
            .collect();
        let meta = |k: &str| match k {
            "names" => Some(r#"{0: 'person', 1: "men's", 2: 'a\'b'}"#.to_string()),
            "character" => Some("0\n1".to_string()),
            _ => None,
        };
        let s = sources(&files, &meta);
        let get = |l: LabelSource| labels(Some(&Labels::From(l)), &s);
        assert_eq!(
            get(LabelSource::File("l.txt".into())).unwrap(),
            ["cat", "dog"]
        );
        assert_eq!(
            get(LabelSource::HfConfig("config.json".into())).unwrap(),
            ["cat", "dog", "", "fox"]
        );
        assert_eq!(
            get(LabelSource::ModelMetadata("names".into())).unwrap(),
            ["person", "men's", "a'b"]
        );
        assert!(
            get(LabelSource::ModelMetadata("other".into()))
                .unwrap_err()
                .contains("no \"other\"")
        );
        assert!(
            get(LabelSource::File("missing.txt".into()))
                .unwrap_err()
                .contains("isn't among")
        );
        assert_eq!(
            charset(Some(&TextSource::File("keys.txt".into())), &s).unwrap(),
            ["a", "b", " "]
        );
        assert_eq!(
            charset(Some(&TextSource::ModelMetadata("character".into())), &s).unwrap(),
            ["0", "1"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_dicts_are_refused() {
        for bad in ["[0]", "{x: 'a'}", "{0: a}", "{0: 'a", "{99999999: 'a'}"] {
            assert!(python_dict(bad).is_err(), "{bad}");
        }
        assert_eq!(python_dict("{}").unwrap(), Vec::<String>::new());
    }
}
