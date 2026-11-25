//! A stalled mint HTTP connection must fail instead of hanging forever.
#![cfg(not(target_arch = "wasm32"))]

use std::io::Read;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use cdk_http_client::{HttpClient, HttpError};

#[tokio::test]
async fn request_times_out_when_server_never_responds() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
            std::thread::sleep(Duration::from_secs(5));
        }
    });

    let client = HttpClient::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .expect("client");
    let started = Instant::now();
    let result = client.get_raw(&format!("http://{address}/hang")).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "hanging HTTP request must time out promptly, took {elapsed:?}"
    );
    assert!(
        matches!(result, Err(HttpError::Timeout)),
        "expected Timeout, got {result:?}"
    );

    let _ = server.join();
}

#[tokio::test]
async fn default_client_has_a_bounded_request_timeout() {
    assert_eq!(
        cdk_http_client::DEFAULT_REQUEST_TIMEOUT,
        Duration::from_secs(30)
    );
}
