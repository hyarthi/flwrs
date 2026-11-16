//! The ticker checked with trellis, as a dev-dependency: the protocol audit and
//! the behaviour tests in `tests/trellis/`, against the binary cargo built for
//! this test. A ticker emits on its own, so no stimulus is needed.

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use std::path::Path;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ticker"))
}

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let target = target()
        .with_launch_args(serde_json::json!({ "interval": "100ms" }))
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

async fn run(file: &str) {
    let mut tester = Tester::new(target());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/trellis")
        .join(file);
    let result = tester.run_file(&path).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test]
async fn follows_credit_and_counts_missed_ticks() {
    run("credit.test.toml").await;
}

#[tokio::test]
async fn ticks_on_a_cron_schedule() {
    run("cron.test.toml").await;
}
