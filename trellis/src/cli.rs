//! The command line: `flwrs-trellis <package|audit|test|bench> …`.
//!
//! [`main`] is the whole program, so a plugin repo can run a pinned trellis
//! from a dev-dependency with a one-line example
//! (`fn main() -> std::process::ExitCode { flwrs_trellis::cli::main() }`)
//! and an alias (`trellis = "run --quiet --example trellis --"`).

use crate::audit::{Audit, CASES, Outcome};
use crate::bench::{BenchFile, BenchResult, Bencher};
use crate::config::Config;
use crate::package::{PackageOptions, package};
use crate::stimulus::{CommandStimulus, Vars, free_port, substitute, substitute_json};
use crate::target::Target;
use crate::testing::{TestResult, Tester};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "flwrs-trellis",
    version,
    about = "Developer toolkit for flwrs plugins: package, audit, test, bench"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the plugin and package it as a ZIP the server accepts.
    Package {
        /// The plugin folder (with manifest.json).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Cross-build for this Rust target triple.
        #[arg(long)]
        target: Option<String>,
        /// A flavor from trellis.toml.
        #[arg(long)]
        flavor: Option<String>,
        /// A debug build (default: release).
        #[arg(long)]
        debug: bool,
        /// Where the ZIP goes.
        #[arg(long)]
        out: Option<PathBuf>,
        #[command(flatten)]
        common: ConfigArg,
    },
    /// Check the plugin against the plugin protocol (docs/protocol.md).
    Audit {
        #[command(flatten)]
        plugin: PluginArgs,
        /// Run only these cases (repeatable). See --list.
        #[arg(long = "case")]
        cases: Vec<String>,
        /// List the cases and exit.
        #[arg(long)]
        list: bool,
        /// Extra arguments for the plugin, after the bootstrap ones.
        #[arg(last = true)]
        plugin_args: Vec<String>,
    },
    /// Run behavioural tests (*.test.toml), or a smoke test without any.
    Test {
        #[command(flatten)]
        plugin: PluginArgs,
        /// Test files (default: trellis.toml [test] files, else tests/*.test.toml).
        files: Vec<PathBuf>,
        /// Write actual outputs into the expected payload files.
        #[arg(long)]
        update: bool,
        #[arg(long, value_enum, default_value_t = TestFormat::Text)]
        format: TestFormat,
    },
    /// Run the plugin's configurator (manifest `configurator`) the way the
    /// server does, on --launch-args (and --upload files), and print its result.
    Configure {
        #[command(flatten)]
        plugin: PluginArgs,
    },
    /// Measure timings and resource use (*.bench.toml, or a default closed loop).
    Bench {
        #[command(flatten)]
        plugin: PluginArgs,
        /// Bench files (default: trellis.toml [bench] files, else benches/*.bench.toml).
        files: Vec<PathBuf>,
        /// Earlier results (--format json) to compare with.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Fail when throughput drops or p99 latency grows by more than this (%).
        #[arg(long, default_value_t = 10.0)]
        max_regression: f64,
        #[arg(long, value_enum, default_value_t = BenchFormat::Table)]
        format: BenchFormat,
    },
}

#[derive(Args)]
struct ConfigArg {
    /// A trellis.toml other than <plugin folder>/trellis.toml.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct PluginArgs {
    /// A plugin folder (built and packaged first if needed) or a package ZIP.
    path: PathBuf,
    /// A JSON file with the node's launch_args.
    #[arg(long)]
    launch_args: Option<PathBuf>,
    /// A file uploaded to the node, for `"source": "upload"` launch args
    /// (repeatable; on top of trellis.toml's [uploads]).
    #[arg(long = "upload", value_name = "NAME=PATH", value_parser = parse_var)]
    uploads: Vec<(String, String)>,
    /// Show the plugin's stdout and stderr.
    #[arg(long)]
    show_output: bool,
    /// `${NAME}` → VALUE in payload files, launch args and commands (repeatable).
    #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var)]
    vars: Vec<(String, String)>,
    /// Build Rust plugins in debug or release (default: debug, release for bench).
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[command(flatten)]
    config: ConfigArg,
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Debug,
    Release,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TestFormat {
    Text,
    Junit,
    Json,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum BenchFormat {
    Table,
    Json,
}

/// The program: parses the arguments and runs the command.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("couldn't start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli.command)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Package {
            dir,
            target,
            flavor,
            debug,
            out,
            common,
        } => {
            let packaged = package(&PackageOptions {
                dir,
                target,
                flavor,
                release: !debug,
                out,
                config: common.config,
                target_dir: None,
            })?;
            println!("{}", packaged.path.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Audit {
            plugin,
            cases,
            list,
            plugin_args,
        } => {
            if list {
                for (name, checks) in CASES {
                    println!("{name:<22} {checks}");
                }
                return Ok(ExitCode::SUCCESS);
            }
            let (mut target, config) = open(&plugin, Profile::Debug)?;
            let port = free_port();
            target = with_audit_launch_args(target, &plugin, config.as_ref(), port)?;
            if let Some(config) = &config {
                target
                    .args
                    .extend(config.audit.args.iter().map(|a| substitute(a, port)));
            }
            target.args.extend(plugin_args);
            target.env.push(("TRELLIS_PORT".into(), port.to_string()));
            let mut audit = Audit::new(target).only(&cases).port(port);
            if let Some(stimulus) = config.as_ref().and_then(command_stimulus) {
                audit = audit.stimulus(stimulus);
            }
            let results = audit.run().await;
            print!("{}", crate::audit::report(&results));
            Ok(exit(
                results
                    .iter()
                    .any(|r| matches!(r.outcome, Outcome::Fail(_))),
            ))
        }
        Command::Test {
            plugin,
            files,
            update,
            format,
        } => {
            let (mut target, config) = open(&plugin, Profile::Debug)?;
            if let Some(path) = &plugin.launch_args {
                target = target.with_launch_args(read_json(path)?)?;
            }
            let files = if files.is_empty() {
                match &config {
                    Some(c) => c.test_files()?,
                    None => vec![],
                }
            } else {
                files
            };
            let mut tester = Tester::new(target.clone()).update(update);
            for (k, v) in vars(&plugin) {
                tester = tester.var(k, v);
            }
            if let Some(stimulus) = config.as_ref().and_then(command_stimulus) {
                tester = tester.stimulus(stimulus);
            }
            let mut results: Vec<TestResult> = vec![];
            if files.is_empty() {
                if config
                    .as_ref()
                    .and_then(|c| c.audit.launch_args.as_ref())
                    .is_some()
                {
                    let config = config.as_ref().expect("checked");
                    let port = free_port();
                    let target = with_audit_launch_args(target, &plugin, Some(config), port)?;
                    tester = Tester::new(target).update(update);
                    for (k, v) in vars(&plugin) {
                        tester = tester.var(k, v);
                    }
                    if let Some(stimulus) = command_stimulus(config) {
                        tester = tester.stimulus(stimulus);
                    }
                }
                results.push(tester.smoke().await);
            } else {
                for file in &files {
                    results.push(tester.run_file(file).await);
                }
            }
            match format {
                TestFormat::Text => print!("{}", crate::testing::report(&results)),
                TestFormat::Junit => print!("{}", crate::testing::junit(&results)),
                TestFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?
                ),
            }
            Ok(exit(
                results
                    .iter()
                    .any(|r| matches!(r.outcome, Outcome::Fail(_))),
            ))
        }
        Command::Configure { plugin } => {
            let (target, _) = open(&plugin, Profile::Debug)?;
            let args = match &plugin.launch_args {
                Some(path) => read_json(path)?,
                None => serde_json::json!({}),
            };
            let ran = tokio::task::spawn_blocking(move || {
                target.configure(&args).map(|c| {
                    let files: Vec<serde_json::Value> = c
                        .files
                        .iter()
                        .map(|f| {
                            let size = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
                            serde_json::json!({"field": f.field, "name": f.name, "size": size})
                        })
                        .collect();
                    serde_json::json!({"values": c.values, "files": files, "warnings": c.warnings})
                })
            })
            .await
            .map_err(|e| e.to_string())?;
            match ran {
                Ok(result) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(errors) => {
                    for e in errors {
                        eprintln!("{e}");
                    }
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::Bench {
            plugin,
            files,
            baseline,
            max_regression,
            format,
        } => {
            let (mut target, config) = open(&plugin, Profile::Release)?;
            if let Some(path) = &plugin.launch_args {
                target = target.with_launch_args(read_json(path)?)?;
            }
            let files = if files.is_empty() {
                match &config {
                    Some(c) => c.bench_files()?,
                    None => vec![],
                }
            } else {
                files
            };
            let mut bencher = Bencher::new(target);
            for (k, v) in vars(&plugin) {
                bencher = bencher.var(k, v);
            }
            let mut results: Vec<BenchResult> = vec![];
            if files.is_empty() {
                let mut file = BenchFile::default_named("default");
                if let Some(config) = &config {
                    file.dir = config.dir.clone();
                    file.launch_args = config.audit.launch_args.clone();
                    file.stimulus = config.audit.stimulus.clone();
                }
                results.push(bencher.run(&file).await?);
            } else {
                for file in &files {
                    results.push(bencher.run_file(file).await?);
                }
            }
            match format {
                BenchFormat::Table => print!("{}", crate::bench::report(&results)),
                BenchFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?
                ),
            }
            if let Some(path) = baseline {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let base: Vec<BenchResult> =
                    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                let regressions = crate::bench::compare(&base, &results, max_regression);
                for r in &regressions {
                    eprintln!("regression: {r}");
                }
                return Ok(exit(!regressions.is_empty()));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn exit(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The plugin to run, and its trellis.toml (if any). A folder that needs a
/// build (a Rust crate, or `[build]` in trellis.toml) or whose executable
/// isn't there is packaged first, so the run uses exactly what would ship.
fn open(plugin: &PluginArgs, default_profile: Profile) -> Result<(Target, Option<Config>), String> {
    let path = &plugin.path;
    let config = match &plugin.config.config {
        Some(file) => Some(Config::read(file)?),
        None if path.is_dir() => Some(Config::load(path)?),
        None => None,
    };
    let mut target = if path.is_file() {
        Target::from_zip(path)?
    } else {
        let buildable =
            config.as_ref().is_some_and(|c| c.build.is_some()) || path.join("Cargo.toml").is_file();
        if buildable {
            let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
            let profile = plugin.profile.unwrap_or(default_profile);
            let packaged = package(&PackageOptions {
                dir: path.clone(),
                target: None,
                flavor: None,
                release: matches!(profile, Profile::Release),
                out: Some(temp.path().to_path_buf()),
                config: plugin.config.config.clone(),
                target_dir: None,
            })?;
            eprintln!("packaged {}", packaged.path.display());
            Target::from_zip(&packaged.path)?
        } else {
            Target::from_package(path)?
        }
    };
    target.show_output = plugin.show_output;
    if let Some(config) = &config {
        for (name, path) in &config.uploads {
            target = target.with_upload(name, config.dir.join(path))?;
        }
    }
    for (name, path) in &plugin.uploads {
        target = target.with_upload(name, path)?;
    }
    Ok((target, config))
}

/// launch_args from --launch-args, else trellis.toml's [audit].
fn with_audit_launch_args(
    target: Target,
    plugin: &PluginArgs,
    config: Option<&Config>,
    port: u16,
) -> Result<Target, String> {
    let file = match (
        &plugin.launch_args,
        config.and_then(|c| c.audit.launch_args.as_ref()),
    ) {
        (Some(path), _) => Some(path.clone()),
        (None, Some(rel)) => config.map(|c| c.dir.join(rel)),
        (None, None) => None,
    };
    match file {
        Some(path) => target.with_launch_args(substitute_json(&read_json(&path)?, port)),
        None => Ok(target),
    }
}

fn command_stimulus(config: &Config) -> Option<CommandStimulus> {
    config.audit.stimulus.as_ref().map(|argv| CommandStimulus {
        argv: argv.clone(),
        cwd: config.dir.clone(),
        vars: Vars::new(),
    })
}

fn parse_var(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{text:?} isn't NAME=VALUE"))
}

fn vars(plugin: &PluginArgs) -> Vars {
    plugin.vars.iter().cloned().collect()
}

fn read_json(path: &Path) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}
