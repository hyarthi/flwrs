use crate::args::CmdArgs;
use crate::config::toml_source::TomlSource;
use clap::Parser;
use serde::de::DeserializeOwned;
use std::sync::{Arc, LazyLock};

mod toml_source;

pub trait Source: Sync + Send {
    fn read_string(&self, path: &[String]) -> Option<String>;
    fn read_int(&self, path: &[String]) -> Option<i64>;
    fn read_bool(&self, path: &[String]) -> Option<bool>;
    fn read_float(&self, path: &[String]) -> Option<f64>;
    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>>;
}

pub enum ESource {
    Toml(TomlSource),
}

impl Source for ESource {
    fn read_string(&self, path: &[String]) -> Option<String> {
        match self {
            ESource::Toml(s) => s.read_string(path),
        }
    }

    fn read_int(&self, path: &[String]) -> Option<i64> {
        match self {
            ESource::Toml(s) => s.read_int(path),
        }
    }

    fn read_bool(&self, path: &[String]) -> Option<bool> {
        match self {
            ESource::Toml(s) => s.read_bool(path),
        }
    }

    fn read_float(&self, path: &[String]) -> Option<f64> {
        match self {
            ESource::Toml(s) => s.read_float(path),
        }
    }

    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>> {
        match self {
            ESource::Toml(s) => s.sub(path),
        }
    }
}

pub trait SourceReader<T: DeserializeOwned> {
    fn read(&self, path: &[String]) -> Option<T>;
}

static MAIN_CONFIG: LazyLock<ESource> = LazyLock::new(init_main_config);

fn init_main_config() -> ESource {
    let args = CmdArgs::parse();
    match toml_source::new_toml_source(args.config) {
        Ok(source) => ESource::Toml(source),
        Err(e) => {
            // The logger isn't up yet (it is configured from this file).
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Main config, loaded from the `--config` path on first use.
pub fn main_config() -> &'static ESource {
    &MAIN_CONFIG
}

pub fn read_struct<T: DeserializeOwned>(source: &ESource, path: &[String]) -> Option<T> {
    match source {
        ESource::Toml(s) => toml_source::read(s, path),
    }
}
