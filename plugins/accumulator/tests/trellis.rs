//! The accumulator checked with trellis, as a dev-dependency: the
//! protocol audit and the behaviour tests in `tests/trellis/`, against the
//! binary cargo built for this test.

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use std::path::Path;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-accumulator"))
}

fn launch_args() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis/sum.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let target = target().with_launch_args(launch_args()).unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test]
async fn closes_windows_on_time() {
    let mut tester = Tester::new(target());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis/session.test.toml");
    let result = tester.run_file(&path).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}
