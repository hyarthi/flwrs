//! The WebSocket source's configurator (manifest `configurator`, contract 1;
//! see `docs/plugin-spec.md`): derives the node's `out_schema` from its
//! `asyncapi` setting. Run by the server in a temporary folder (or behind a
//! remote node's endpoint): `--request request.json --response response.json`.

use flwrs_ws_source::{convert, spec};
use serde_json::{Value, json};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let flag = |name: &str| {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let (Some(request), Some(response)) = (flag("--request"), flag("--response")) else {
        eprintln!("usage: flwrs-ws-source-config --request <file> --response <file>");
        return ExitCode::from(2);
    };
    let (answer, ok) = match configure(&request) {
        Ok((schema, warnings)) => (
            json!({"values": {"out_schema": schema}, "warnings": warnings}),
            true,
        ),
        Err(errors) => (json!({"errors": errors}), false),
    };
    if let Err(e) = std::fs::write(&response, answer.to_string()) {
        eprintln!("{response}: {e}");
        return ExitCode::FAILURE;
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn configure(request: &str) -> Result<(Value, Vec<String>), Vec<String>> {
    let text = std::fs::read_to_string(request).map_err(|e| vec![format!("{request}: {e}")])?;
    let request: Value =
        serde_json::from_str(&text).map_err(|e| vec![format!("{request}: {e}")])?;
    if request.get("contract").and_then(Value::as_u64) != Some(1) {
        return Err(vec!["this configurator speaks contract 1".into()]);
    }
    match request
        .pointer("/launch_args/asyncapi")
        .filter(|v| !v.is_null())
    {
        None => Ok((convert::to_json(&convert::out_schema(None)?), vec![])),
        Some(setting) => {
            let api = spec::load(spec::parse(setting).map_err(|e| vec![e])?)?;
            let schema = convert::out_schema(Some(&api))?;
            Ok((convert::to_json(&schema), api.warnings))
        }
    }
}
