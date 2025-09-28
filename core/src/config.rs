use crate::args::CmdArgs;
use crate::config::toml_source::TomlSource;
use clap::Parser;
use lazy_static::lazy_static;
use serde::de::DeserializeOwned;
use std::sync::Arc;

mod toml_source;

pub trait Source: Sync + Send {
    fn read_string(&self, path: &[String]) -> Option<String>;
    fn read_int(&self, path: &[String]) -> Option<i64>;
    fn read_bool(&self, path: &[String]) -> Option<bool>;
    fn read_float(&self, path: &[String]) -> Option<f64>;
    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>>;
}

pub enum ESource {
    Toml(TomlSource)
}

impl Source for ESource {
    fn read_string(&self, path: &[String]) -> Option<String> {
        match self { ESource::Toml(s) => { s.read_string(path) } }
    }

    fn read_int(&self, path: &[String]) -> Option<i64> {
        match self { ESource::Toml(s) => { s.read_int(path) } }
    }

    fn read_bool(&self, path: &[String]) -> Option<bool> {
        match self { ESource::Toml(s) => { s.read_bool(path) } }
    }

    fn read_float(&self, path: &[String]) -> Option<f64> {
        match self { ESource::Toml(s) => { s.read_float(path) } }
    }

    fn sub(&self, path: &[String]) -> Option<Arc<dyn Source>> {
        match self { ESource::Toml(s) => { s.sub(path) } }
    }
}

pub trait SourceReader<T: DeserializeOwned> {
    fn read(&self, path: &[String]) -> Option<T>;
}

lazy_static! {
    static ref MAIN_CONFIG: Arc<ESource> = init_main_config();
}

fn init_main_config() -> Arc<ESource> {
    let args = CmdArgs::parse();

    let main_source = toml_source::new_toml_source(args.config).unwrap();

    Arc::new(ESource::Toml(main_source))
}

pub fn main_config() -> &'static ESource {
    MAIN_CONFIG.as_ref()
}

pub fn read_struct<T: DeserializeOwned>(source: &ESource, path: &[String]) -> Option<T> {
    match source { ESource::Toml(s) => { toml_source::read(s, path) } }
}
