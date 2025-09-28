use super::Source;
use serde::de::DeserializeOwned;
use std::fs;
use std::sync::Arc;
use toml::value::Table;
use toml::Value;

pub(crate) fn new_toml_source(file_path: String) -> Result<TomlSource, String> {
    let contents = match fs::read_to_string(&file_path) {
        Ok(file) => file,
        Err(e) => {
            return Err(format!(
                "failed to open file[{file}]: {err}",
                file = file_path,
                err = e.to_string()
            ))
        }
    };
    let source = match contents.parse::<Value>() {
        Ok(v) => v,
        Err(e) => {
            return Err(format!(
                "failed to parse file[{file}]: {err}",
                file = file_path,
                err = e.to_string()
            ))
        }
    };
    let delegate = match source {
        Value::String(_) => {
            return Err(format!(
                "file[{file}] contents is a string",
                file = file_path
            ))
        }
        Value::Integer(_) => {
            return Err(format!(
                "file[{file}] contents is an integer",
                file = file_path
            ))
        }
        Value::Float(_) => {
            return Err(format!(
                "file[{file}] contents is a float",
                file = file_path
            ))
        }
        Value::Boolean(_) => {
            return Err(format!(
                "file[{file}] contents is a boolean",
                file = file_path
            ))
        }
        Value::Datetime(_) => {
            return Err(format!(
                "file[{file}] contents is a datetime",
                file = file_path
            ))
        }
        Value::Array(_) => {
            return Err(format!(
                "file[{file}] contents is an array",
                file = file_path
            ))
        }
        Value::Table(t) => t,
    };
    Ok(TomlSource { delegate })
}

pub struct TomlSource {
    delegate: Table,
}

impl Source for TomlSource {
    fn read_string(&self, path: &[String]) -> Option<String> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first().unwrap());
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        field.and_then(|x| x.as_str().map(|x| x.to_string()))
    }

    fn read_int(&self, path: &[String]) -> Option<i64> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first().unwrap());
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        field.and_then(|val| val.as_integer())
    }

    fn read_bool(&self, path: &[String]) -> Option<bool> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first().unwrap());
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        field.and_then(|x| x.as_bool())
    }

    fn read_float(&self, path: &[String]) -> Option<f64> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first().unwrap());
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        field.and_then(|x| x.as_float())
    }

    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first()?);
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        Some(Arc::new(TomlSource {
            delegate: field?.as_table()?.clone(),
        }))
    }
}

impl TomlSource {
    fn sub_internal(&self, path: &[String]) -> Option<TomlSource> {
        if path.len() == 0 {
            return None;
        }
        let mut field = self.delegate.get(path.first()?);
        for i in 1..path.len() {
            field = field?.as_table()?.get(path[i].as_str());
        }
        Some(TomlSource {
            delegate: field?.as_table()?.clone(),
        })
    }
}

// impl<T: DeserializeOwned> SourceReader<T> for TomlSource {
//     fn read(&self, path: &[String]) -> Option<T> {
//         if path.len() == 0 {
//             return None;
//         }
//         let mut opt_field = self.delegate.get(path.first()?);
//         for i in 1..path.len() {
//             opt_field = opt_field?.as_table()?.get(path[i].as_str());
//         }
//         if opt_field == None {
//             return None;
//         }
//         let field = opt_field?;
//         let ser_field = match toml::to_string(field) {
//             Ok(f) => f,
//             Err(_) => {
//                 return None;
//             }
//         };
//         match toml::from_str::<T>(ser_field.as_str()) {
//             Ok(v) => Some(v),
//             Err(_) => None,
//         }
//     }
// }

pub fn read<T: DeserializeOwned>(source: &TomlSource, path: &[String]) -> Option<T> {
    let field = if path.len() == 0 { source.delegate.clone() } else { source.sub_internal(path)?.delegate.clone() };
    let ser_field = match toml::to_string(&field) {
        Ok(f) => f,
        Err(_) => {
            return None;
        }
    };
    match toml::from_str::<T>(ser_field.as_str()) {
        Ok(v) => Some(v),
        Err(e) => {
            println!("error: {}", e);
            None
        },
    }
}
