//! `flwrs-trellis <plugin folder> [-- extra plugin args]`: checks a
//! plugin package against the plugin protocol (docs/protocol.md).

use clap::Parser;
use flwrs_trellis::{CASES, Outcome, Target, report, run};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "flwrs-trellis",
    about = "Checks that a flwrs plugin follows the plugin protocol",
    long_about = "Runs the plugin in a package folder (with manifest.json at its root) against a fake hub, once per case, and checks the rules of the plugin protocol (docs/protocol.md). Exits with status 1 if a case fails."
)]
struct Args {
    /// The plugin's package folder (with manifest.json).
    package: PathBuf,
    /// Run only these cases (repeatable). See --list.
    #[arg(long = "case")]
    cases: Vec<String>,
    /// List the cases and exit.
    #[arg(long)]
    list: bool,
    /// Extra arguments for the plugin, after the bootstrap ones.
    #[arg(last = true)]
    plugin_args: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if args.list {
        for (name, checks) in CASES {
            println!("{name:<22} {checks}");
        }
        return ExitCode::SUCCESS;
    }
    let mut target = match Target::from_package(&args.package) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("{}: {err}", args.package.display());
            return ExitCode::FAILURE;
        }
    };
    target.args = args.plugin_args;
    let results = run(&target, &args.cases).await;
    print!("{}", report(&results));
    if results.iter().any(|r| matches!(r.outcome, Outcome::Fail(_))) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
