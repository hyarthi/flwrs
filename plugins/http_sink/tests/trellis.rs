//! http_sink checked with trellis, as a dev-dependency: the protocol audit and
//! the behaviour tests in `tests/trellis/`, against the binary cargo built for
//! this test. This is the pattern for plugin crates (see trellis/README.md).

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-http-sink"))
}

/// A tiny HTTP server: `/ok` → 200, `/busy` → 503, anything else → 400.
async fn mock_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = vec![];
                let mut buf = [0u8; 4096];
                // The request line and headers are enough to answer.
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&request);
                let status = if line.starts_with("POST /ok ") {
                    "200 OK"
                } else if line.starts_with("POST /busy ") {
                    "503 Service Unavailable"
                } else {
                    "400 Bad Request"
                };
                let response =
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let results = Audit::new(target()).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test]
async fn acks_and_nacks_by_http_outcome() {
    let port = mock_server().await;
    let mut tester = Tester::new(target()).var("mock", port.to_string());
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis/delivery.test.toml");
    let result = tester.run_file(&file).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}
