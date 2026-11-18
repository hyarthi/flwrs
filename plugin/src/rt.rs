//! The plugin's tokio runtime, sized by the server: `--threads N` (see the
//! plugin specification: the manifest's `threads` says how a plugin takes
//! it; this SDK's plugins declare `{"via": "arg", "name": "threads"}`) is the
//! number of cores to use: `1` a single-threaded runtime, `N` the main
//! thread plus `N - 1` workers, and `0` or no argument tokio's default (a
//! worker per core). It lets a device or server keep cores for other work.
//!
//! Parse the arguments first, then build the runtime:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     let args = Args::parse(); // flattens flwrs_plugin::cli::Bootstrap
//!     match flwrs_plugin::rt::runtime(args.bootstrap.threads) {
//!         Ok(runtime) => runtime.block_on(run(args)),
//!         Err(e) => {
//!             eprintln!("starting the runtime: {e}");
//!             std::process::ExitCode::FAILURE
//!         }
//!     }
//! }
//! ```

/// A runtime for `threads` cores (`Some(1)`: single-threaded; `None` or
/// `Some(0)`: tokio's default).
pub fn runtime(threads: Option<usize>) -> std::io::Result<tokio::runtime::Runtime> {
    match threads.unwrap_or(0) {
        0 => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build(),
        1 => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        n => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(n - 1)
            .enable_all()
            .build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::RuntimeFlavor;

    #[test]
    fn builds_each_kind() {
        for (threads, flavor, workers) in [
            (None, RuntimeFlavor::MultiThread, None),
            (Some(0), RuntimeFlavor::MultiThread, None),
            (Some(1), RuntimeFlavor::CurrentThread, Some(1)),
            (Some(3), RuntimeFlavor::MultiThread, Some(2)),
        ] {
            let rt = runtime(threads).unwrap();
            assert_eq!(rt.handle().runtime_flavor(), flavor);
            if let Some(n) = workers {
                assert_eq!(rt.metrics().num_workers(), n, "{threads:?}");
            }
            assert_eq!(rt.block_on(async { 1 + 1 }), 2);
        }
    }
}
