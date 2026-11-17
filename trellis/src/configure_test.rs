//! Tests of a plugin's configurator (manifest `configurator`): `*.configure.toml`
//! files, run by `trellis test` (and `Tester::run_file`) next to the
//! behavioural `*.test.toml` ones. Each runs the configurator once, the way
//! the server does, and checks its result.
//!
//! ```toml
//! # tests/devices.configure.toml
//! name = "derives the out schema from the OpenAPI document"
//! launch_args = "devices.json"                 # the node's settings (configured fields left out)
//! uploads = { "api.yaml" = "api.yaml" }        # files uploaded to the node, optional
//! expect = "ok"                                # or "error"
//! values = "expected/devices.values.json"      # optional: the configured values, exactly
//! warnings_contain = ["no requestBody"]        # optional
//! # errors_contain = ["paths./x: oneOf"]       # with expect = "error"
//! ```
//!
//! `--update` (or `Tester::update(true)`) writes the actual values into the
//! `values` file instead of comparing. `${name}` variables apply to the
//! launch args, as in test files.

use crate::audit::Outcome;
use crate::stimulus::{Vars, substitute_json_vars};
use crate::target::Target;
use crate::testing::{StepResult, TestResult};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The file-name suffix that makes a test file a configurator test.
pub const SUFFIX: &str = ".configure.toml";

#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Expect {
    #[default]
    Ok,
    Error,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ConfigureTestFile {
    pub name: Option<String>,
    pub launch_args: Option<String>,
    #[serde(default)]
    pub uploads: BTreeMap<String, String>,
    #[serde(default)]
    pub expect: Expect,
    pub values: Option<String>,
    #[serde(default)]
    pub warnings_contain: Vec<String>,
    #[serde(default)]
    pub errors_contain: Vec<String>,
}

pub fn is_configure_test(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(SUFFIX))
}

/// Runs one configurator test file.
pub async fn run_file(target: &Target, path: &Path, vars: &Vars, update: bool) -> TestResult {
    let started = Instant::now();
    let name = path.display().to_string();
    let result = run(target, path, vars, update).await;
    let (name, steps, outcome) = match result {
        Ok((name, steps)) => {
            let failed = steps.iter().find_map(|s: &StepResult| match &s.outcome {
                Outcome::Fail(e) => Some(format!("{}: {e}", s.action)),
                _ => None,
            });
            (name, steps, failed.map_or(Outcome::Pass, Outcome::Fail))
        }
        Err(e) => (name, vec![], Outcome::Fail(e)),
    };
    TestResult {
        name,
        file: Some(path.to_path_buf()),
        outcome,
        steps,
        logs: vec![],
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

async fn run(
    target: &Target,
    path: &Path,
    vars: &Vars,
    update: bool,
) -> Result<(String, Vec<StepResult>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: ConfigureTestFile =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = file
        .name
        .clone()
        .unwrap_or_else(|| path.display().to_string());
    if target.manifest.configurator.is_none() {
        return Err("the plugin has no configurator (manifest `configurator`)".into());
    }
    let args = match &file.launch_args {
        Some(rel) => {
            let p = dir.join(rel);
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let json: Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
            substitute_json_vars(&json, 0, vars)
        }
        None => Value::Object(Default::default()),
    };
    let mut target = target.clone();
    for (upload, rel) in &file.uploads {
        target = target.with_upload(upload, dir.join(rel))?;
    }
    let ran = tokio::task::spawn_blocking(move || {
        target
            .configure(&args)
            .map(|c| (c.values.clone(), c.warnings.clone()))
    })
    .await
    .map_err(|e| format!("the configurator's thread failed: {e}"))?;
    let mut steps = vec![];
    let mut step = |action: &str, outcome: Outcome| {
        steps.push(StepResult {
            index: steps.len() + 1,
            action: action.into(),
            outcome,
        });
    };
    match (&file.expect, ran) {
        (Expect::Ok, Err(errors)) => step(
            "configure",
            Outcome::Fail(format!("expected success, got: {}", errors.join("; "))),
        ),
        (Expect::Error, Ok(_)) => step(
            "configure",
            Outcome::Fail("expected an error, but it succeeded".into()),
        ),
        (Expect::Error, Err(errors)) => {
            step("configure", Outcome::Pass);
            for want in &file.errors_contain {
                let found = errors.iter().any(|e| e.contains(want.as_str()));
                step(
                    &format!("errors contain {want:?}"),
                    if found {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(format!("not in {errors:?}"))
                    },
                );
            }
        }
        (Expect::Ok, Ok((values, warnings))) => {
            step("configure", Outcome::Pass);
            for want in &file.warnings_contain {
                let found = warnings.iter().any(|w| w.contains(want.as_str()));
                step(
                    &format!("warnings contain {want:?}"),
                    if found {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(format!("not in {warnings:?}"))
                    },
                );
            }
            if let Some(rel) = &file.values {
                let expected_path: PathBuf = dir.join(rel);
                let actual = Value::Object(values);
                if update {
                    if let Some(parent) = expected_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    let mut text = serde_json::to_string_pretty(&actual).expect("JSON serializes");
                    text.push('\n');
                    std::fs::write(&expected_path, text)
                        .map_err(|e| format!("{}: {e}", expected_path.display()))?;
                    step("values (updated)", Outcome::Pass);
                } else {
                    let expected: Value = std::fs::read_to_string(&expected_path)
                        .map_err(|e| {
                            format!(
                                "{}: {e} (run with --update to write it)",
                                expected_path.display()
                            )
                        })
                        .and_then(|t| {
                            serde_json::from_str(&t)
                                .map_err(|e| format!("{}: {e}", expected_path.display()))
                        })?;
                    let outcome = if expected == actual {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(format!(
                            "the configured values differ from {}: {}",
                            expected_path.display(),
                            serde_json::to_string(&actual).unwrap_or_default()
                        ))
                    };
                    step("values", outcome);
                }
            }
        }
    }
    Ok((name, steps))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::Tester;
    use std::os::unix::fs::PermissionsExt;

    /// A package folder whose configurator sets `out_schema` from `name`
    /// ("bad" fails), warning once.
    fn package() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        };
        write(
            "manifest.json",
            r#"{"manifest_version": 2, "id": "conf", "name": "Conf", "version": "1.0.0", "type": "SOURCE",
                "executable": "bin/run", "launch_args_schema": "schema/launch.json",
                "payload_schemas": {"out": {"from_launch_args": "/out_schema"}},
                "configurator": {"executable": "bin/configure", "fields": ["out_schema"]}}"#,
        );
        write(
            "schema/launch.json",
            r#"{"type": "object", "additionalProperties": false,
                "properties": {"name": {"type": "string"}, "out_schema": {"type": "object"}}}"#,
        );
        write("bin/run", "#!/bin/sh\n");
        let script = write(
            "bin/configure",
            r#"#!/bin/sh
name=$(sed -n 's/.*"name": "\([^"]*\)".*/\1/p' request.json)
[ "$name" = bad ] && { echo '{"errors": ["name is bad"]}' > response.json; exit 1; }
printf '{"values": {"out_schema": {"fields": [{"key": "%s", "type": "STRING"}]}}, "warnings": ["derived"]}' "$name" > response.json
"#,
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[tokio::test]
    async fn targets_run_the_configurator_and_configure_tests_check_it() {
        let pkg = package();
        let target = Target::from_package(pkg.path()).unwrap();
        // with_launch_args configures first: the out schema comes from it.
        let configured = target
            .clone()
            .with_launch_args(serde_json::json!({"name": "temp"}))
            .unwrap();
        assert_eq!(
            configured.out_schema.as_ref().unwrap().fields[0].key,
            "temp"
        );
        assert_eq!(
            configured.configured_values().unwrap()["out_schema"]["fields"][0]["key"],
            "temp"
        );
        // Values for configured fields are refused, as on the server.
        let e = target
            .configure(&serde_json::json!({"name": "x", "out_schema": {}}))
            .unwrap_err();
        assert!(e[0].contains("set by the plugin's configurator"), "{e:?}");

        let tests = pkg.path().join("tests");
        std::fs::create_dir(&tests).unwrap();
        std::fs::write(tests.join("temp.json"), r#"{"name": "temp"}"#).unwrap();
        std::fs::write(tests.join("bad.json"), r#"{"name": "bad"}"#).unwrap();
        std::fs::write(
            tests.join("ok.configure.toml"),
            "launch_args = \"temp.json\"\nvalues = \"expected/temp.values.json\"\nwarnings_contain = [\"derived\"]\n",
        )
        .unwrap();
        std::fs::write(
            tests.join("fails.configure.toml"),
            "launch_args = \"bad.json\"\nexpect = \"error\"\nerrors_contain = [\"name is bad\"]\n",
        )
        .unwrap();
        // --update writes the snapshot, then it's compared.
        let mut tester = Tester::new(target.clone()).update(true);
        assert_eq!(
            tester
                .run_file(&tests.join("ok.configure.toml"))
                .await
                .outcome,
            Outcome::Pass
        );
        let mut tester = Tester::new(target.clone());
        for file in ["ok.configure.toml", "fails.configure.toml"] {
            let result = tester.run_file(&tests.join(file)).await;
            assert_eq!(
                result.outcome,
                Outcome::Pass,
                "{file}: {}",
                crate::testing::report(std::slice::from_ref(&result))
            );
        }
        // A changed snapshot fails.
        std::fs::write(
            tests.join("expected/temp.values.json"),
            r#"{"out_schema": {}}"#,
        )
        .unwrap();
        let result = tester.run_file(&tests.join("ok.configure.toml")).await;
        assert!(matches!(result.outcome, Outcome::Fail(e) if e.contains("differ")));
    }
}
