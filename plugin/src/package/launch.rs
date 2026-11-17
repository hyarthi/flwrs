//! How a node's `launch_args` reach its plugin: the launch-args schema's
//! annotations ([`LaunchSpec`]), their checks, and the launch plan
//! ([`LaunchSpec::plan`]: command-line arguments, environment variables and
//! the files the server writes to the node's `config/` folder).
//!
//! The contract is specified in `docs/plugin-spec.md`. Manifest version 2
//! reads these keywords of the launch-args JSON Schema:
//!
//! - `writeOnly: true` on a top-level property: a secret (delivered by `env`
//!   or `file`, never on the command line).
//! - `x-flwrs-launch` on a top-level property: `{"via": "arg" | "env" | "file",
//!   "alias"?, "file_name"?, "source"?: "upload"}`.
//! - `x-flwrs-code` on a string property: `{"language", "params", "definitions"?, "signature"?}`.
//! - UI hints: `x-flwrs-group`, `x-flwrs-order`, `x-flwrs-advanced`, `x-flwrs-widget`.
//!
//! Version 1 manifests ignore them; their `launch_args` all go on the command
//! line, as before.

use super::manifest::{
    LaunchArgsStyle, Manifest, RESERVED_ARGS, SchemaSource, check_relative_path,
    launch_args_to_argv, parse_pointer,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The longest command line a node may have, in UTF-16 units as Windows
/// counts it (its limit is 32 767), checked on every platform so a scene runs
/// anywhere.
pub const MAX_COMMAND_LINE: usize = 30_000;
/// The longest value of one environment variable (same reasoning).
pub const MAX_ENV_VALUE: usize = 30_000;
/// The node folder's subfolder for the files the server provides.
pub const CONFIG_DIR: &str = "config";
/// What a file path counts as in the command-line limit when the node's
/// folder isn't known (Windows' classic `MAX_PATH`).
const PATH_ALLOWANCE: usize = 260;
/// Room kept for the executable and the bootstrap arguments.
const BOOTSTRAP_ALLOWANCE: usize = 512;

/// Environment variables a plugin's field can't be delivered in: the
/// server's own (`FLWRS_*`) and ones that change how programs are found,
/// loaded or run.
const RESERVED_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "PWD",
    "SHELL",
    "USER",
    "USERNAME",
    "LOGNAME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "IFS",
    "ENV",
    "BASH_ENV",
    "COMSPEC",
    "SYSTEMROOT",
    "WINDIR",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "PERL5LIB",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "CLASSPATH",
];
const RESERVED_ENV_PREFIXES: &[&str] = &["FLWRS_", "LD_", "DYLD_"];

/// The UI hints a server knows; other `x-flwrs-*` keywords are warnings.
const HINTS: &[&str] = &[
    "x-flwrs-group",
    "x-flwrs-order",
    "x-flwrs-advanced",
    "x-flwrs-widget",
];
const WIDGETS: &[&str] = &["duration", "cron", "file-path", "code"];

/// How a field reaches the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// `--<flag> <value>` (`true` → `--<flag>`; `false` and `null` → nothing).
    Arg,
    /// An environment variable.
    Env,
    /// A file in the node's `config/` folder; the plugin gets `--<flag> <path>`.
    File,
}

/// A top-level property of the launch-args schema and how it's delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchField {
    pub name: String,
    pub via: Via,
    /// The flag without `--` (`Arg`, `File`) or the variable's name (`Env`).
    pub target: String,
    /// The name in `config/` (`File`, not for uploads).
    pub file_name: Option<String>,
    /// The value names a file uploaded to the node (`File`).
    pub upload: bool,
    /// `writeOnly: true`.
    pub secret: bool,
}

/// A code field (`x-flwrs-code`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeField {
    /// JSON pointer of the field in `launch_args`.
    pub pointer: String,
    pub language: String,
    /// The names the code is called with (it's a function body).
    pub params: Vec<String>,
    /// A file in the package with type definitions for an editor.
    pub definitions: Option<String>,
    pub signature: Option<String>,
}

/// The launch rules of a plugin: its manifest's style plus, for manifest
/// version 2, what its launch-args schema declares.
#[derive(Clone, Debug)]
pub struct LaunchSpec {
    v2: bool,
    style: LaunchArgsStyle,
    fields: BTreeMap<String, LaunchField>,
    code: Vec<CodeField>,
    input_pointer: Option<String>,
    output_pointer: Option<String>,
    /// The fields the plugin's configurator sets.
    configured: Vec<String>,
}

/// A file the server writes to (or links into) the node's `config/` folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    /// Its name in `config/`.
    pub name: String,
    pub content: ConfigContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigContent {
    /// Written from the node's config.
    Data(Vec<u8>),
    /// A link to the node's uploaded file with this name.
    Upload(String),
}

/// What a launch needs besides the executable and the bootstrap arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub files: Vec<ConfigFile>,
}

/// Schema problems: errors refuse the package, warnings don't.
#[derive(Debug, Default)]
pub struct SchemaReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl LaunchSpec {
    /// The launch rules of `manifest`, whose launch-args schema is `schema`.
    /// Errors and warnings are about the schema's annotations (and, for
    /// per-node payload schemas, the manifest's pointers into it).
    pub fn new(manifest: &Manifest, schema: Option<&Value>) -> (LaunchSpec, SchemaReport) {
        let mut report = SchemaReport::default();
        let v2 = manifest.manifest_version >= 2;
        let pointer =
            |s: &Option<SchemaSource>| s.as_ref().and_then(|s| s.pointer()).map(String::from);
        let mut spec = LaunchSpec {
            v2,
            style: manifest.launch_args_style,
            fields: BTreeMap::new(),
            code: vec![],
            input_pointer: pointer(&manifest.payload_schemas.input),
            output_pointer: pointer(&manifest.payload_schemas.output),
            configured: manifest
                .configurator
                .as_ref()
                .map(|c| c.fields.clone())
                .unwrap_or_default(),
        };
        let Some(schema) = schema.filter(|_| v2) else {
            return (spec, report);
        };
        walk(schema, "", Place::Root, &mut spec.code, &mut report);
        if spec.style == LaunchArgsStyle::Positional {
            if !spec.configured.is_empty() {
                report
                    .errors
                    .push("configurator needs launch_args_style \"flags\"".into());
            }
            if annotated(schema) {
                report.errors.push(
                    "launch_args_schema: x-flwrs-launch, x-flwrs-code and writeOnly need launch_args_style \"flags\""
                        .into(),
                );
            }
            return (spec, report);
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                match field(name, prop) {
                    Ok(f) => {
                        spec.fields.insert(name.clone(), f);
                    }
                    Err(errors) => report.errors.extend(errors),
                }
            }
        }
        check_unique(&spec.fields, &mut report.errors);
        for name in &spec.configured {
            match spec.fields.get(name) {
                None => report.errors.push(format!(
                    "configurator.fields: {name:?} isn't a top-level property of the launch_args_schema"
                )),
                Some(f) if f.secret => report.errors.push(format!(
                    "configurator.fields: {name:?} is a secret (writeOnly): a configurator's output is stored and shown"
                )),
                Some(_) => {}
            }
        }
        for (label, ptr) in [
            ("payload_schemas.in", &spec.input_pointer),
            ("payload_schemas.out", &spec.output_pointer),
        ] {
            if let Some(ptr) = ptr
                && let Err(e) = check_schema_pointer(schema, ptr)
            {
                report
                    .errors
                    .push(format!("{label}.from_launch_args {ptr:?}: {e}"));
            }
        }
        (spec, report)
    }

    /// The rules of a plugin without a launch-args schema, `flags` style.
    pub fn flags() -> LaunchSpec {
        LaunchSpec {
            v2: false,
            style: LaunchArgsStyle::Flags,
            fields: BTreeMap::new(),
            code: vec![],
            input_pointer: None,
            output_pointer: None,
            configured: vec![],
        }
    }

    /// The fields the plugin's configurator sets (empty without one).
    pub fn configured_fields(&self) -> &[String] {
        &self.configured
    }

    /// Whether `name` is set by the configurator.
    pub fn is_configured(&self, name: &str) -> bool {
        self.configured.iter().any(|c| c == name)
    }

    /// The field named `name`.
    pub fn field(&self, name: &str) -> Option<&LaunchField> {
        self.fields.get(name)
    }

    /// The declared fields with their delivery (manifest version 2).
    pub fn fields(&self) -> impl Iterator<Item = &LaunchField> {
        self.fields.values()
    }

    /// JSON pointers of the secret fields (`writeOnly`).
    pub fn secrets(&self) -> Vec<String> {
        self.fields
            .values()
            .filter(|f| f.secret)
            .map(|f| format!("/{}", escape(&f.name)))
            .collect()
    }

    pub fn code_fields(&self) -> &[CodeField] {
        &self.code
    }

    /// JSON pointers of the per-node payload schemas (`in`, `out`).
    pub fn schema_pointers(&self) -> (Option<&str>, Option<&str>) {
        (
            self.input_pointer.as_deref(),
            self.output_pointer.as_deref(),
        )
    }

    /// Package files the schema refers to (`x-flwrs-code.definitions`).
    pub fn file_references(&self) -> Vec<(String, &str)> {
        self.code
            .iter()
            .filter_map(|c| {
                let path = c.definitions.as_deref()?;
                Some((
                    format!("launch_args_schema {}: definitions", c.pointer),
                    path,
                ))
            })
            .collect()
    }

    /// The launch plan for a node's `launch_args`; file arguments point into
    /// `config_dir` (the node's `config/` folder, absolute). Every problem is
    /// returned.
    pub fn plan(&self, args: &Value, config_dir: &Path) -> Result<LaunchPlan, Vec<String>> {
        if !self.v2 || self.style == LaunchArgsStyle::Positional {
            return launch_args_to_argv(self.style, args).map(|argv| LaunchPlan {
                argv,
                ..Default::default()
            });
        }
        let empty = Map::new();
        let map = match args {
            Value::Null => &empty,
            Value::Object(map) => map,
            _ => return Err(vec!["launch_args must be a JSON object".into()]),
        };
        let mut plan = LaunchPlan::default();
        let mut errors = vec![];
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let value = &map[key];
            if value.is_null() {
                continue;
            }
            let field = match self.fields.get(key) {
                Some(f) => f.clone(),
                None => match undeclared(key, value) {
                    Ok(f) => f,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                },
            };
            if let Err(e) = deliver(&field, value, config_dir, &mut plan) {
                errors.push(format!("launch_args/{key}: {e}"));
            }
        }
        let mut names = HashSet::new();
        for f in &plan.files {
            if !names.insert(f.name.as_str()) {
                errors.push(format!(
                    "two launch_args fields deliver a file named {:?} in config/",
                    f.name
                ));
            }
        }
        if errors.is_empty() {
            Ok(plan)
        } else {
            Err(errors)
        }
    }

    /// Everything a node's `launch_args` must satisfy apart from the JSON
    /// Schema: the conversion rules and the size limits, checked as if the
    /// node folder's path were as long as Windows allows.
    pub fn check(&self, args: &Value) -> Vec<String> {
        let placeholder = PathBuf::from("x".repeat(PATH_ALLOWANCE - CONFIG_DIR.len() - 1));
        match self.plan(args, &placeholder.join(CONFIG_DIR)) {
            Err(errors) => errors,
            Ok(plan) => {
                let mut errors = vec![];
                let length = BOOTSTRAP_ALLOWANCE + command_line_length(&plan.argv);
                if length > MAX_COMMAND_LINE {
                    errors.push(format!(
                        "launch_args make a command line of about {length} characters; the limit is {MAX_COMMAND_LINE} (deliver long values with \"via\": \"file\")"
                    ));
                }
                for (name, value) in &plan.env {
                    let length = value.encode_utf16().count();
                    if length > MAX_ENV_VALUE {
                        errors.push(format!(
                            "launch_args: environment variable {name} would be {length} characters; the limit is {MAX_ENV_VALUE}"
                        ));
                    }
                }
                errors
            }
        }
    }
}

/// Validates a node's `launch_args` against the plugin's JSON Schema (if it
/// has one) and its launch rules ([`LaunchSpec::check`]).
pub fn validate_launch_args(
    schema: Option<&jsonschema::Validator>,
    spec: &LaunchSpec,
    args: &Value,
) -> Vec<String> {
    // Omitted launch_args means "no extra arguments": validate the style's
    // empty value, so schemas with required keys still catch the omission.
    let empty = match spec.style {
        LaunchArgsStyle::Flags => Value::Object(Map::new()),
        LaunchArgsStyle::Positional => Value::Array(vec![]),
    };
    let effective = if args.is_null() { &empty } else { args };
    let mut errors: Vec<String> = match schema {
        Some(validator) => validator
            .iter_errors(effective)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() {
                    format!("launch_args: {e}")
                } else {
                    format!("launch_args{at}: {e}")
                }
            })
            .collect(),
        None => vec![],
    };
    errors.extend(spec.check(args));
    errors
}

/// The length of `argv` as one Windows command line (each argument quoted
/// the way Rust's `Command` does it, separated by spaces), in UTF-16 units.
pub fn command_line_length(argv: &[String]) -> usize {
    argv.iter().map(|a| quoted_length(a) + 1).sum()
}

fn quoted_length(arg: &str) -> usize {
    let units = arg.encode_utf16().count();
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return units;
    }
    // Surrounding quotes, plus a backslash before each quote and before each
    // run of backslashes that ends at a quote or at the closing quote.
    let mut extra = 2;
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                extra += backslashes + 1;
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    units + extra + backslashes
}

/// Where the walk is: the schema root, a property reached through a chain of
/// `properties` (with its depth: 1 = top level), or anywhere else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Root,
    Property(usize),
    Other,
}

/// Checks the annotations' places and shapes everywhere in the schema, and
/// collects the code fields.
fn walk(
    node: &Value,
    pointer: &str,
    place: Place,
    code: &mut Vec<CodeField>,
    report: &mut SchemaReport,
) {
    match node {
        Value::Array(items) => {
            for item in items {
                walk(item, pointer, Place::Other, code, report);
            }
        }
        Value::Object(map) => {
            let at = |p: &str| {
                if p.is_empty() {
                    "the schema root".to_string()
                } else {
                    format!("launch_args{p}")
                }
            };
            let property = matches!(place, Place::Property(_));
            for (key, value) in map {
                if key == "writeOnly" && value == &Value::Bool(true) && place != Place::Property(1)
                {
                    report.errors.push(format!(
                        "launch_args_schema at {}: writeOnly (a secret) is only allowed on top-level properties",
                        at(pointer)
                    ));
                }
                let Some(_) = key.strip_prefix("x-flwrs-") else {
                    continue;
                };
                match key.as_str() {
                    "x-flwrs-launch" if place != Place::Property(1) => report.errors.push(format!(
                        "launch_args_schema at {}: x-flwrs-launch is only allowed on top-level properties",
                        at(pointer)
                    )),
                    "x-flwrs-launch" => {} // read by `field`
                    "x-flwrs-code" if !property => report.errors.push(format!(
                        "launch_args_schema at {}: x-flwrs-code is only allowed on properties",
                        at(pointer)
                    )),
                    "x-flwrs-code" => match code_field(pointer, map, value) {
                        Ok(c) => code.push(c),
                        Err(e) => report
                            .errors
                            .push(format!("launch_args_schema at {}: x-flwrs-code: {e}", at(pointer))),
                    },
                    hint if HINTS.contains(&hint) => {
                        if let Err(e) = check_hint(hint, value) {
                            report
                                .errors
                                .push(format!("launch_args_schema at {}: {hint}: {e}", at(pointer)));
                        }
                    }
                    other => report.warnings.push(format!(
                        "launch_args_schema at {}: {other} isn't known to this server and is ignored",
                        at(pointer)
                    )),
                }
            }
            for (key, value) in map {
                if key.starts_with("x-flwrs-") {
                    continue;
                }
                if key == "properties"
                    && matches!(place, Place::Root | Place::Property(_))
                    && let Value::Object(props) = value
                {
                    let depth = match place {
                        Place::Property(d) => d + 1,
                        _ => 1,
                    };
                    for (name, prop) in props {
                        let p = format!("{pointer}/{}", escape(name));
                        walk(prop, &p, Place::Property(depth), code, report);
                    }
                } else {
                    walk(value, pointer, Place::Other, code, report);
                }
            }
        }
        _ => {}
    }
}

/// Whether a schema uses any of the annotations that need named fields.
fn annotated(node: &Value) -> bool {
    match node {
        Value::Array(items) => items.iter().any(annotated),
        Value::Object(map) => map.iter().any(|(k, v)| {
            k == "x-flwrs-launch"
                || k == "x-flwrs-code"
                || (k == "writeOnly" && v == &Value::Bool(true))
                || annotated(v)
        }),
        _ => false,
    }
}

fn check_hint(hint: &str, value: &Value) -> Result<(), String> {
    match hint {
        "x-flwrs-group" => match value.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(()),
            _ => Err("must be a non-empty string".into()),
        },
        "x-flwrs-order" => value
            .as_i64()
            .map(|_| ())
            .ok_or_else(|| "must be an integer".into()),
        "x-flwrs-advanced" => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| "must be true or false".into()),
        "x-flwrs-widget" => match value.as_str() {
            Some(w) if WIDGETS.contains(&w) => Ok(()),
            _ => Err(format!("must be one of {}", WIDGETS.join(", "))),
        },
        _ => Ok(()),
    }
}

fn code_field(
    pointer: &str,
    property: &Map<String, Value>,
    value: &Value,
) -> Result<CodeField, String> {
    if types(property).iter().any(|t| t != "string") {
        return Err("the property must be a string".into());
    }
    let Some(spec) = value.as_object() else {
        return Err("must be an object".into());
    };
    let known = ["language", "params", "definitions", "signature"];
    if let Some(k) = spec.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(format!("unknown key {k:?}"));
    }
    let language = match spec.get("language").and_then(Value::as_str) {
        Some("lua") => "lua".to_string(),
        Some(other) => return Err(format!("language {other:?} isn't supported (lua)")),
        None => return Err("language is required".into()),
    };
    let params = match spec.get("params") {
        None => vec![],
        Some(Value::Array(items)) => {
            let mut params = vec![];
            for item in items {
                match item.as_str() {
                    Some(p) if is_identifier(p) && !params.iter().any(|q| q == p) => {
                        params.push(p.to_string())
                    }
                    _ => return Err(format!("params: {item} isn't a distinct identifier")),
                }
            }
            params
        }
        Some(_) => return Err("params must be a list of names".into()),
    };
    let text = |key: &str| -> Result<Option<String>, String> {
        match spec.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(format!("{key} must be a non-empty string")),
        }
    };
    let definitions = text("definitions")?;
    if let Some(d) = &definitions {
        check_relative_path(d).map_err(|e| format!("definitions {d:?}: {e}"))?;
    }
    Ok(CodeField {
        pointer: pointer.to_string(),
        language,
        params,
        definitions,
        signature: text("signature")?,
    })
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `type` keyword as a list (empty when absent).
fn types(property: &Map<String, Value>) -> Vec<String> {
    match property.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

/// A declared top-level property's delivery.
fn field(name: &str, prop: &Value) -> Result<LaunchField, Vec<String>> {
    let at = format!("launch_args_schema at launch_args/{name}");
    let empty = Map::new();
    let prop = prop.as_object().unwrap_or(&empty);
    let types = types(prop);
    let structured = types.iter().any(|t| t == "object" || t == "array");
    let secret = prop.get("writeOnly") == Some(&Value::Bool(true));
    let mut errors = vec![];
    let launch = match prop.get("x-flwrs-launch") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(vec![format!("{at}: x-flwrs-launch must be an object")]),
    };
    if let Some(k) = launch
        .keys()
        .find(|k| !["via", "alias", "file_name", "source"].contains(&k.as_str()))
    {
        errors.push(format!("{at}: x-flwrs-launch: unknown key {k:?}"));
    }
    let via = match launch.get("via").map(|v| v.as_str()) {
        None if structured => Via::File,
        None if secret => {
            errors.push(format!(
                "{at}: a secret (writeOnly) must say how it's delivered: \"via\": \"env\" or \"file\""
            ));
            Via::Env
        }
        None => Via::Arg,
        Some(Some("arg")) => Via::Arg,
        Some(Some("env")) => Via::Env,
        Some(Some("file")) => Via::File,
        Some(_) => {
            return Err(vec![format!(
                "{at}: x-flwrs-launch.via must be \"arg\", \"env\" or \"file\""
            )]);
        }
    };
    if structured && via != Via::File {
        errors.push(format!(
            "{at}: objects and arrays can only be delivered as files (\"via\": \"file\")"
        ));
    }
    if secret && via == Via::Arg {
        errors.push(format!(
            "{at}: a secret (writeOnly) can't be a command-line argument (anyone on the host can read those); use \"env\" or \"file\""
        ));
    }
    let upload = match launch.get("source").map(|v| v.as_str()) {
        None => false,
        Some(Some("upload")) => true,
        Some(_) => {
            errors.push(format!("{at}: x-flwrs-launch.source must be \"upload\""));
            false
        }
    };
    if upload && (via != Via::File || structured) {
        errors.push(format!(
            "{at}: \"source\": \"upload\" needs \"via\": \"file\" and a string property (the upload's name)"
        ));
    }
    let alias = match launch.get("alias") {
        None => None,
        Some(Value::String(a)) => Some(a.clone()),
        Some(_) => {
            errors.push(format!("{at}: x-flwrs-launch.alias must be a string"));
            None
        }
    };
    let target = match via {
        Via::Env => {
            let name = alias.unwrap_or_else(|| name.replace('-', "_").to_ascii_uppercase());
            if let Err(e) = check_env_name(&name) {
                errors.push(format!("{at}: environment variable {name:?}: {e}"));
            }
            name
        }
        Via::Arg | Via::File => {
            let flag = alias.unwrap_or_else(|| name.replace('_', "-"));
            if let Err(e) = check_flag(&flag) {
                errors.push(format!("{at}: flag --{flag}: {e}"));
            }
            flag
        }
    };
    let file_name = match (via, upload, launch.get("file_name")) {
        (Via::File, false, Some(Value::String(f))) => Some(f.clone()),
        (Via::File, false, None) => Some(default_file_name(name, prop, structured)),
        (_, _, None) => None,
        (_, _, Some(_)) => {
            errors.push(format!(
                "{at}: x-flwrs-launch.file_name is only for \"via\": \"file\" (not uploads), as a string"
            ));
            None
        }
    };
    if let Some(f) = &file_name
        && let Err(e) = check_file_name(f)
    {
        errors.push(format!("{at}: file name {f:?}: {e}"));
    }
    if errors.is_empty() {
        Ok(LaunchField {
            name: name.to_string(),
            via,
            target,
            file_name,
            upload,
            secret,
        })
    } else {
        Err(errors)
    }
}

/// A field the schema doesn't declare (when it allows extra properties):
/// values go on the command line, objects and arrays as JSON files.
fn undeclared(key: &str, value: &Value) -> Result<LaunchField, String> {
    let flag = key.replace('_', "-");
    check_flag(&flag).map_err(|e| format!("launch_args/{key}: flag --{flag}: {e}"))?;
    let structured = value.is_object() || value.is_array();
    Ok(LaunchField {
        name: key.to_string(),
        via: if structured { Via::File } else { Via::Arg },
        target: flag,
        file_name: structured.then(|| format!("{key}.json")),
        upload: false,
        secret: false,
    })
}

fn default_file_name(name: &str, prop: &Map<String, Value>, structured: bool) -> String {
    let media = prop
        .get("contentMediaType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ext = match media {
        "text/x-lua" => ".lua",
        "application/json" => ".json",
        "application/yaml" | "application/x-yaml" | "text/yaml" | "text/x-yaml" => ".yaml",
        "application/toml" => ".toml",
        "application/x-pem-file" => ".pem",
        "application/xml" | "text/xml" => ".xml",
        "text/csv" => ".csv",
        "text/plain" => ".txt",
        _ if structured => ".json",
        _ => "",
    };
    format!("{name}{ext}")
}

/// Delivers one value.
fn deliver(
    field: &LaunchField,
    value: &Value,
    config_dir: &Path,
    plan: &mut LaunchPlan,
) -> Result<(), String> {
    let text = |value: &Value| -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    let flag = format!("--{}", field.target);
    match field.via {
        Via::Arg => match value {
            Value::Bool(true) => plan.argv.push(flag),
            Value::Bool(false) => {}
            v => {
                let t = text(v).ok_or("objects and arrays can only be delivered as files")?;
                plan.argv.extend([flag, t]);
            }
        },
        Via::Env => {
            let t = text(value).ok_or("objects and arrays can only be delivered as files")?;
            plan.env.push((field.target.clone(), t));
        }
        Via::File => {
            let (name, content) = if field.upload {
                let name = value
                    .as_str()
                    .ok_or("must be the name of a file uploaded to the node")?;
                check_file_name(name).map_err(|e| format!("upload name {name:?}: {e}"))?;
                (name.to_string(), ConfigContent::Upload(name.to_string()))
            } else {
                let bytes = match value {
                    Value::Object(_) | Value::Array(_) => {
                        let mut json =
                            serde_json::to_vec_pretty(value).expect("a JSON value serializes");
                        json.push(b'\n');
                        json
                    }
                    v => text(v).unwrap_or_default().into_bytes(),
                };
                let name = field
                    .file_name
                    .clone()
                    .expect("set for every non-upload file field");
                (name, ConfigContent::Data(bytes))
            };
            plan.argv
                .extend([flag, config_dir.join(&name).to_string_lossy().into_owned()]);
            plan.files.push(ConfigFile { name, content });
        }
    }
    Ok(())
}

fn check_unique(fields: &BTreeMap<String, LaunchField>, errors: &mut Vec<String>) {
    let mut flags: HashMap<&str, &str> = HashMap::new();
    let mut envs: HashMap<&str, &str> = HashMap::new();
    let mut files: HashMap<&str, &str> = HashMap::new();
    for f in fields.values() {
        let (map, what) = match f.via {
            Via::Env => (&mut envs, "environment variable"),
            _ => (&mut flags, "flag"),
        };
        if let Some(other) = map.insert(&f.target, &f.name) {
            errors.push(format!(
                "launch_args_schema: launch_args/{other} and launch_args/{} use the same {what} {:?}",
                f.name, f.target
            ));
        }
        if let Some(file) = &f.file_name
            && let Some(other) = files.insert(file, &f.name)
        {
            errors.push(format!(
                "launch_args_schema: launch_args/{other} and launch_args/{} use the same file name {file:?}",
                f.name
            ));
        }
    }
}

/// A flag a field can be delivered with: letters, digits and `-`, and none
/// of the bootstrap arguments.
pub fn check_flag(flag: &str) -> Result<(), &'static str> {
    if flag.is_empty()
        || flag.starts_with('-')
        || !flag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("may only contain letters, digits, '_' and '-'");
    }
    if RESERVED_ARGS.contains(&flag) {
        return Err("is set by the hub and can't be overridden");
    }
    Ok(())
}

/// An environment variable a field can be delivered in.
pub fn check_env_name(name: &str) -> Result<(), &'static str> {
    let mut bytes = name.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_');
    if !first_ok || !bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
        return Err("must match ^[A-Z_][A-Z0-9_]*$");
    }
    if RESERVED_ENV.contains(&name) || RESERVED_ENV_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err("is reserved (the server's own, or one that changes how programs run)");
    }
    Ok(())
}

/// A file name in `config/` (and of an upload): letters, digits, `.`, `_`
/// and `-`, 1 to 100 characters, not starting with `.`.
pub fn check_file_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 100 {
        return Err("must be 1 to 100 characters");
    }
    if name.starts_with('.') {
        return Err("must not start with '.'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("may only contain letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

/// Checks that a per-node schema's pointer leads, through `properties`, to an
/// object property of the launch-args schema.
fn check_schema_pointer(schema: &Value, pointer: &str) -> Result<(), String> {
    let tokens = parse_pointer(pointer).map_err(String::from)?;
    let mut node = schema;
    for token in &tokens {
        node = node
            .get("properties")
            .and_then(|p| p.get(token))
            .ok_or_else(|| format!("launch_args_schema has no property {token:?} there"))?;
    }
    let props = node.as_object().cloned().unwrap_or_default();
    if types(&props) != ["object"] {
        return Err("must point at a property with \"type\": \"object\"".into());
    }
    Ok(())
}

/// Escapes a JSON pointer token.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::manifest::{FromLaunchArgs, PayloadSchemas, PluginType};
    use serde_json::json;

    fn manifest(version: u32) -> Manifest {
        Manifest {
            manifest_version: version,
            id: "script".into(),
            name: "Script".into(),
            version: "0.1.0".into(),
            plugin_type: PluginType::Transform,
            description: None,
            executable: "bin/script".into(),
            platforms: None,
            launch_args_schema: Some("schema/launch_args.json".into()),
            launch_args_style: LaunchArgsStyle::Flags,
            payload_schemas: PayloadSchemas {
                input: Some(SchemaSource::FromLaunchArgs(FromLaunchArgs {
                    from_launch_args: "/schemas/in".into(),
                    default: None,
                })),
                output: Some(SchemaSource::Path("schema/out.json".into())),
            },
            runtime_defaults: None,
            ui: None,
            configurator: None,
            extensions: Default::default(),
        }
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "schemas": {"type": "object", "properties": {"in": {"type": "object"}}},
                "script": {
                    "type": "string", "contentMediaType": "text/x-lua",
                    "x-flwrs-launch": {"via": "file"},
                    "x-flwrs-code": {"language": "lua", "params": ["payload", "meta"], "definitions": "types/script.d.lua"}
                },
                "model": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
                "api_key": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "env", "alias": "SCRIPT_API_KEY"}},
                "client_key": {"type": "string", "writeOnly": true, "contentMediaType": "application/x-pem-file",
                               "x-flwrs-launch": {"via": "file", "alias": "key"}},
                "timeout_ms": {"type": "integer", "x-flwrs-group": "Limits", "x-flwrs-advanced": true},
                "verbose": {"type": "boolean"},
                "region": {"type": "string", "x-flwrs-launch": {"via": "env"}, "x-flwrs-widget": "cron"}
            }
        })
    }

    fn spec() -> LaunchSpec {
        let (spec, report) = LaunchSpec::new(&manifest(2), Some(&schema()));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        spec
    }

    #[test]
    fn reads_the_annotations() {
        let spec = spec();
        let f = |n: &str| spec.fields.get(n).unwrap().clone();
        assert_eq!(f("schemas").via, Via::File);
        assert_eq!(f("schemas").file_name.as_deref(), Some("schemas.json"));
        assert_eq!(f("script").file_name.as_deref(), Some("script.lua"));
        assert!(f("model").upload && f("model").file_name.is_none());
        assert_eq!(
            (f("api_key").via, f("api_key").target.as_str()),
            (Via::Env, "SCRIPT_API_KEY")
        );
        assert_eq!(f("client_key").file_name.as_deref(), Some("client_key.pem"));
        assert_eq!(f("client_key").target, "key");
        assert_eq!(
            (f("timeout_ms").via, f("timeout_ms").target.as_str()),
            (Via::Arg, "timeout-ms")
        );
        assert_eq!(f("region").target, "REGION");
        assert_eq!(spec.secrets(), ["/api_key", "/client_key"]);
        assert_eq!(spec.code_fields().len(), 1);
        assert_eq!(spec.code_fields()[0].pointer, "/script");
        assert_eq!(spec.code_fields()[0].params, ["payload", "meta"]);
        assert_eq!(spec.schema_pointers(), (Some("/schemas/in"), None));
        assert_eq!(spec.file_references()[0].1, "types/script.d.lua");
    }

    #[test]
    fn plans_a_launch() {
        let args = json!({
            "schemas": {"in": {"fields": []}},
            "script": "return payload",
            "model": "resnet50.onnx",
            "api_key": "s3cr3t",
            "timeout_ms": 100,
            "verbose": true,
            "region": null,
            "extra_flag": "x",
            "extra_obj": {"a": 1}
        });
        let plan = spec().plan(&args, Path::new("/n/config")).unwrap();
        let p = |name: &str| {
            Path::new("/n/config")
                .join(name)
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            plan.argv,
            [
                "--extra-flag".to_string(),
                "x".into(),
                "--extra-obj".into(),
                p("extra_obj.json"),
                "--model".into(),
                p("resnet50.onnx"),
                "--schemas".into(),
                p("schemas.json"),
                "--script".into(),
                p("script.lua"),
                "--timeout-ms".into(),
                "100".into(),
                "--verbose".into(),
            ]
        );
        assert_eq!(
            plan.env,
            [("SCRIPT_API_KEY".to_string(), "s3cr3t".to_string())]
        );
        let names: Vec<_> = plan.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "extra_obj.json",
                "resnet50.onnx",
                "schemas.json",
                "script.lua"
            ]
        );
        assert_eq!(
            plan.files[1].content,
            ConfigContent::Upload("resnet50.onnx".into())
        );
        assert_eq!(
            plan.files[3].content,
            ConfigContent::Data(b"return payload".to_vec())
        );
        let ConfigContent::Data(json) = &plan.files[2].content else {
            panic!()
        };
        assert_eq!(
            serde_json::from_slice::<Value>(json).unwrap(),
            json!({"in": {"fields": []}})
        );
    }

    #[test]
    fn plan_refuses_bad_values() {
        let spec = spec();
        let errors = spec
            .plan(
                &json!({"api_key": {"a": 1}, "model": "../x", "token": "t", "bad key": 1}),
                Path::new("/c"),
            )
            .unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:?}");
        // an upload named like a generated file
        let errors = spec
            .plan(
                &json!({"model": "script.lua", "script": "x"}),
                Path::new("/c"),
            )
            .unwrap_err();
        assert!(errors[0].contains("script.lua"), "{errors:?}");
        assert!(spec.plan(&json!([1]), Path::new("/c")).is_err());
    }

    #[test]
    fn checks_the_size_limits() {
        let spec = spec();
        assert!(spec.check(&json!({"region": "eu"})).is_empty());
        let long = "x".repeat(MAX_COMMAND_LINE + 1);
        let errors = spec.check(&json!({"extra": long}));
        assert!(errors[0].contains("command line"), "{errors:?}");
        let errors = spec.check(&json!({"region": long}));
        assert!(errors[0].contains("REGION"), "{errors:?}");
        // a long value in a file is fine
        assert!(spec.check(&json!({"script": long})).is_empty());
    }

    #[test]
    fn windows_quoting_lengths() {
        assert_eq!(quoted_length("abc"), 3);
        assert_eq!(quoted_length(""), 2);
        assert_eq!(quoted_length("a b"), 5);
        assert_eq!(quoted_length(r#"a"b"#), 6); // "a\"b"
        assert_eq!(quoted_length(r#"a\"b c"#), 10); // "a\\\"b c"
        assert_eq!(quoted_length(r"a b\"), 7); // "a b\\"
        assert_eq!(quoted_length(r"a\b"), 3);
    }

    #[test]
    fn refuses_bad_annotations() {
        let bad = json!({
            "type": "object",
            "properties": {
                "pw": {"type": "string", "writeOnly": true},
                "pw2": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "arg"}},
                "obj": {"type": "object", "x-flwrs-launch": {"via": "env"},
                        "properties": {"inner": {"type": "string", "writeOnly": true,
                                                 "x-flwrs-launch": {"via": "file"}}}},
                "path": {"type": "string", "x-flwrs-launch": {"via": "env", "alias": "LD_PRELOAD"}},
                "id_": {"type": "string", "x-flwrs-launch": {"alias": "id"}},
                "up": {"type": "integer", "x-flwrs-launch": {"via": "arg", "source": "upload"}},
                "a": {"type": "string", "x-flwrs-launch": {"via": "file", "file_name": "same.txt"}},
                "b": {"type": "string", "x-flwrs-launch": {"via": "file", "file_name": "same.txt"}},
                "c": {"type": "integer", "x-flwrs-code": {"language": "lua"}},
                "d": {"type": "string", "x-flwrs-code": {"language": "python"}},
                "e": {"type": "string", "x-flwrs-widget": "slider", "x-flwrs-order": "1"},
                "f": {"type": "string", "x-flwrs-launch": {"via": "file", "how": 1}}
            },
            "$defs": {"x": {"x-flwrs-code": {"language": "lua"}}}
        });
        let mut m = manifest(2);
        m.payload_schemas.input = None;
        let (_, report) = LaunchSpec::new(&m, Some(&bad));
        for fragment in [
            "launch_args/pw: a secret (writeOnly) must say",
            "launch_args/pw2: a secret (writeOnly) can't be a command-line",
            "launch_args/obj: objects and arrays can only be delivered as files",
            "launch_args/obj/inner: writeOnly",
            "launch_args/obj/inner: x-flwrs-launch is only allowed on top-level",
            "\"LD_PRELOAD\": is reserved",
            "flag --id: is set by the hub",
            "launch_args/up: \"source\": \"upload\" needs",
            "use the same file name \"same.txt\"",
            "launch_args/c: x-flwrs-code: the property must be a string",
            "language \"python\" isn't supported",
            "x-flwrs-widget: must be one of",
            "x-flwrs-order: must be an integer",
            "unknown key \"how\"",
            "the schema root: x-flwrs-code is only allowed on properties",
        ] {
            assert!(
                report.errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?} in {:#?}",
                report.errors
            );
        }
    }

    #[test]
    fn warns_about_unknown_keywords() {
        let s = json!({"type": "object", "properties": {"a": {"type": "string", "x-flwrs-color": "red"}}});
        let mut m = manifest(2);
        m.payload_schemas.input = None;
        let (_, report) = LaunchSpec::new(&m, Some(&s));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report.warnings[0].contains("x-flwrs-color"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn checks_per_node_schema_pointers() {
        let mut m = manifest(2);
        let s = json!({"type": "object", "properties": {"schemas": {"type": "object",
                       "properties": {"in": {"type": "string"}}}}});
        let (_, report) = LaunchSpec::new(&m, Some(&s));
        assert!(
            report.errors[0].contains("\"type\": \"object\""),
            "{:?}",
            report.errors
        );
        m.payload_schemas.input = Some(SchemaSource::FromLaunchArgs(FromLaunchArgs {
            from_launch_args: "/nope".into(),
            default: None,
        }));
        let (_, report) = LaunchSpec::new(&m, Some(&s));
        assert!(
            report.errors[0].contains("no property \"nope\""),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn version_1_ignores_annotations() {
        let (spec, report) = LaunchSpec::new(&manifest(1), Some(&schema()));
        assert!(report.errors.is_empty() && report.warnings.is_empty());
        assert!(spec.secrets().is_empty());
        let plan = spec
            .plan(&json!({"api_key": "k", "verbose": true}), Path::new("/c"))
            .unwrap();
        assert_eq!(plan.argv, ["--api-key", "k", "--verbose"]);
        assert!(plan.env.is_empty() && plan.files.is_empty());
    }

    #[test]
    fn positional_style_has_no_fields() {
        let mut m = manifest(2);
        m.payload_schemas.input = None;
        m.launch_args_style = LaunchArgsStyle::Positional;
        let (spec, report) = LaunchSpec::new(
            &m,
            Some(&json!({"type": "array", "items": {"type": "string"}})),
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            spec.plan(&json!(["run"]), Path::new("/c")).unwrap().argv,
            ["run"]
        );
        let (_, report) = LaunchSpec::new(&m, Some(&schema()));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("need launch_args_style \"flags\""))
        );
    }

    #[test]
    fn env_and_file_names() {
        for ok in ["A", "_X", "MQTT_PASSWORD", "A1"] {
            assert!(check_env_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a",
            "1A",
            "A-B",
            "PATH",
            "FLWRS_TOKEN",
            "LD_LIBRARY_PATH",
            "DYLD_X",
        ] {
            assert!(check_env_name(bad).is_err(), "{bad}");
        }
        for ok in ["a.json", "resnet50.onnx", "README", "x_y-z"] {
            assert!(check_file_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "a b", "..", &"x".repeat(101)] {
            assert!(check_file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_against_the_schema_too() {
        let spec = spec();
        let validator = jsonschema::validator_for(&schema()).unwrap();
        assert!(
            validate_launch_args(Some(&validator), &spec, &json!({"timeout_ms": 5})).is_empty()
        );
        let errors = validate_launch_args(Some(&validator), &spec, &json!({"timeout_ms": "x"}));
        assert!(
            errors[0].starts_with("launch_args/timeout_ms:"),
            "{errors:?}"
        );
        let required =
            jsonschema::validator_for(&json!({"type": "object", "required": ["a"]})).unwrap();
        assert!(!validate_launch_args(Some(&required), &spec, &Value::Null).is_empty());
    }
}
