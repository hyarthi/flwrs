//! The vision source checked with trellis, as a dev-dependency: the protocol
//! audit against the binary cargo built for this test. It emits nothing yet,
//! so the data-events case is skipped.

use flwrs_trellis::{Audit, Outcome, Target};
use std::path::Path;

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let target = Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .with_launch_args(serde_json::json!({
            "graph": { "nodes": { "camera": { "kind": "source" } } }
        }))
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results
            .iter()
            .all(|r| matches!(r.outcome, Outcome::Pass | Outcome::Skip(_))),
        "{}",
        flwrs_trellis::report(&results)
    );
}
