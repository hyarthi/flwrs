//! Trellis, the developer toolkit for flwrs plugins, in any language:
//!
//! - [`package`]: builds a plugin and packages it as a ZIP the server accepts.
//! - [`audit`]: the conformance cases. The plugin runs against a fake hub, and
//!   the rules of the plugin protocol (`docs/protocol.md`) are checked.
//! - [`testing`]: behavioural end-to-end tests from `*.test.toml` files.
//! - [`bench`]: timings and resource use of the plugin alone.
//!
//! It works as a command (`flwrs-trellis <command>`; [`cli::main`]) and as a
//! library, e.g. from a plugin's own integration tests:
//!
//! ```rust,ignore
//! use flwrs_trellis::{Audit, Outcome, Target, Tester};
//!
//! let target = Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))?
//!     .with_executable(env!("CARGO_BIN_EXE_my-plugin"));
//! let results = Audit::new(target.clone()).run().await;
//! assert!(results.iter().all(|r| r.outcome != Outcome::Fail(..)));
//! let result = Tester::new(target).run_file(Path::new("tests/basic.test.toml")).await;
//! ```
//!
//! Sources that only emit when the outside world does something get a
//! [`stimulus::Stimulus`]: any async closure.

pub mod audit;
pub mod bench;
pub mod cli;
pub mod config;
pub mod configure_test;
mod hub;
pub mod package;
pub mod payload;
pub mod stimulus;
pub mod target;
pub mod testing;

pub use audit::{Audit, CASES, CaseResult, Outcome, report};
pub use bench::{BenchFile, BenchResult, Bencher};
pub use hub::INSTANCE_ID;
pub use payload::sample_payload;
pub use stimulus::{Correlation, Stimulus, StimulusCtx, free_port};
pub use target::Target;
pub use testing::{TestFile, TestResult, Tester};
