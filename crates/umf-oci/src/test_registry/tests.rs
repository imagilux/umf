//! Unit tests for the `test_registry` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[tokio::test]
async fn v2_root_responds_with_distribution_header() {
    let registry = TestRegistry::start().await.expect("start");
    let endpoint = registry.endpoint().to_string();
    assert!(endpoint.starts_with("127.0.0.1:"));
    // Hit it with a raw TCP HTTP/1.1 request so we don't pull
    // reqwest into umf-oci just for a sanity check.
    let req = format!("GET /v2/ HTTP/1.1\r\nHost: {endpoint}\r\nConnection: close\r\n\r\n");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(&endpoint)
        .await
        .expect("tcp connect");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 200"), "got: {text}");
    // HTTP headers are case-insensitive; axum normalizes to lowercase.
    assert!(
        text.to_lowercase()
            .contains("docker-distribution-api-version: registry/2.0"),
        "got: {text}",
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn post_upload_returns_202_with_location_header() {
    let registry = TestRegistry::start().await.expect("start");
    let endpoint = registry.endpoint().to_string();
    // POST /v2/myrepo/blobs/uploads/ — distribution-spec start-upload.
    let req = format!(
        "POST /v2/myrepo/blobs/uploads/ HTTP/1.1\r\nHost: {endpoint}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(&endpoint)
        .await
        .expect("tcp connect");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 202"), "got: {text}");
    assert!(text.to_lowercase().contains("location:"), "got: {text}");
    registry.shutdown().await;
}

#[test]
fn digest_is_sha256_of_payload() {
    let d = compute_digest(b"hello");
    assert_eq!(
        d,
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}
