use super::Source;
use serde::de::DeserializeOwned;
use std::fs;
use std::sync::Arc;
use toml::Value;
use toml::value::Table;

pub(crate) fn new_toml_source(file_path: String) -> Result<TomlSource, String> {
    let contents = fs::read_to_string(&file_path)
        .map_err(|e| format!("failed to open file[{file_path}]: {e}"))?;
    parse_toml_source(&contents).map_err(|e| format!("failed to parse file[{file_path}]: {e}"))
}

fn parse_toml_source(contents: &str) -> Result<TomlSource, String> {
    let delegate = contents.parse::<Table>().map_err(|e| e.to_string())?;
    Ok(TomlSource { delegate })
}

pub struct TomlSource {
    delegate: Table,
}

impl TomlSource {
    /// Walks `path` through nested tables. An empty path yields `None`.
    fn lookup(&self, path: &[String]) -> Option<&Value> {
        let (first, rest) = path.split_first()?;
        rest.iter()
            .try_fold(self.delegate.get(first)?, |value, key| {
                value.as_table()?.get(key)
            })
    }

    fn sub_table(&self, path: &[String]) -> Option<Table> {
        self.lookup(path)?.as_table().cloned()
    }
}

impl Source for TomlSource {
    fn read_string(&self, path: &[String]) -> Option<String> {
        self.lookup(path)?.as_str().map(str::to_string)
    }

    fn read_int(&self, path: &[String]) -> Option<i64> {
        self.lookup(path)?.as_integer()
    }

    fn read_bool(&self, path: &[String]) -> Option<bool> {
        self.lookup(path)?.as_bool()
    }

    fn read_float(&self, path: &[String]) -> Option<f64> {
        self.lookup(path)?.as_float()
    }

    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>> {
        Some(Arc::new(TomlSource {
            delegate: self.sub_table(path)?,
        }))
    }
}

/// Deserializes the table at `path` (or the whole file for an empty path).
pub fn read<T: DeserializeOwned>(source: &TomlSource, path: &[String]) -> Option<T> {
    let table = if path.is_empty() {
        source.delegate.clone()
    } else {
        source.sub_table(path)?
    };
    match Value::Table(table).try_into::<T>() {
        Ok(v) => Some(v),
        Err(e) => {
            // Usually called before the logger exists, so print directly.
            eprintln!("Config error at [{}]: {e}", path.join("."));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn path(p: &str) -> Vec<String> {
        p.split('.').map(str::to_string).collect()
    }

    const SAMPLE: &str = r#"
        name = "flwrs"
        [http]
        host = "0.0.0.0"
        port = 8080
        [db.main]
        filename = "main.db"
        vacuum = true
        ratio = 0.5
    "#;

    #[test]
    fn reads_scalars_and_nested_values() {
        let src = parse_toml_source(SAMPLE).unwrap();
        assert_eq!(src.read_string(&path("name")).as_deref(), Some("flwrs"));
        assert_eq!(src.read_int(&path("http.port")), Some(8080));
        assert_eq!(
            src.read_string(&path("db.main.filename")).as_deref(),
            Some("main.db")
        );
        assert_eq!(src.read_bool(&path("db.main.vacuum")), Some(true));
        assert_eq!(src.read_float(&path("db.main.ratio")), Some(0.5));
        assert_eq!(src.read_int(&path("http.host")), None, "wrong type");
        assert_eq!(src.read_int(&path("http.missing")), None);
        assert_eq!(src.read_int(&path("name.deeper")), None, "not a table");
        assert_eq!(src.read_int(&[]), None);

        let db = src.sub(&path("db")).unwrap();
        assert_eq!(
            db.read_string(&path("main.filename")).as_deref(),
            Some("main.db")
        );
    }

    #[test]
    fn deserializes_sections() {
        #[derive(Deserialize)]
        struct Http {
            host: String,
            port: u16,
        }
        let src = parse_toml_source(SAMPLE).unwrap();
        let http: Http = read(&src, &path("http")).unwrap();
        assert_eq!((http.host.as_str(), http.port), ("0.0.0.0", 8080));
        assert!(read::<Http>(&src, &path("db")).is_none());
        assert!(read::<Http>(&src, &path("nope")).is_none());
    }

    #[test]
    fn rejects_invalid_toml() {
        assert!(parse_toml_source("not = [valid").is_err());
    }
}
