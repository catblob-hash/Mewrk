//! The real TLS path, rustls over Graviola, against a local server with a throwaway certificate.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::Rust;
use mewrk_aisdk::error::AiError;
use mewrk_aisdk::http::{Cancel, Fetch, Headers, HttpRequest, NetFetch};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// A self-signed P-256 certificate for localhost and 127.0.0.1, valid for a century, generated for this test and
// worth nothing outside it.
const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBPTCB5aADAgECAgkAsAplLSSCSCEwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTI2MTEwNDA0NTEwMloYDzIxMjYxMDExMDQ1MTAyWjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATu8r7V
MkaCij/obv/RY+S11TzERBS2nucBLst2p/TuEKDsPUifcUI7f7jRp6FgeKanEMJQ
ws1jtHzZUBy5dUPAox4wHDAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYI
KoZIzj0EAwIDRwAwRAIgJC0xLXzf7133ejtgqvIj3TU9kQKNnEocxKC3aWxKMkkC
IHXKPq1qJhsWbgn0fdiUI+qh0W1sqBIBSRg+ihtjVZ4i
-----END CERTIFICATE-----";
const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgFoPNeFMNSxrSPbKQ
G+68ZFAUkjYpmeQmph1KjEsdZTqhRANCAATu8r7VMkaCij/obv/RY+S11TzERBS2
nucBLst2p/TuEKDsPUifcUI7f7jRp6FgeKanEMJQws1jtHzZUBy5dUPA
-----END PRIVATE KEY-----";

// Another one, which no test trusts.
const UNTRUSTED_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBPzCB5aADAgECAgkAmUMc+TeA7/EwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTI2MTEwNDA0NTIxOVoYDzIxMjYxMDExMDQ1MjE5WjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAT7fHWK
fJqWKZnnWfIxB/OFuuRAJTtyDmmqeQ9e4XOhg6QFAl8mJcrMdLb/Cl0CjtAkVe/C
GOWOYllRRIMwg95yox4wHDAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYI
KoZIzj0EAwIDSQAwRgIhAJLA2SdGfDFE29azXy3qKmJ83FgvCS5VGl/3uTTIYvcb
AiEA22iM9G0a2UlXjBi9mDy3gmnnfhzmLjla9K1kXynaKas=
-----END CERTIFICATE-----";
const UNTRUSTED_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPcfB3152Hh/Gr7pN
Q8t/sZr8N0vXgm6PUTaS7il2XmChRANCAAT7fHWKfJqWKZnnWfIxB/OFuuRAJTty
DmmqeQ9e4XOhg6QFAl8mJcrMdLb/Cl0CjtAkVe/CGOWOYllRRIMwg95y
-----END PRIVATE KEY-----";

fn der(pem: &str) -> Vec<u8> {
    let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    base64_simd::STANDARD.decode_to_vec(body.as_bytes()).unwrap()
}

fn acceptor(cert: &str, key: &str) -> tokio_rustls::TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(rustls_graviola::default_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der(cert).into()], rustls::pki_types::PrivateKeyDer::Pkcs8(der(key).into()))
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// One HTTPS answer per connection: SSE, chunked in two writes so the stream arrives through TLS records.
fn serve(runtime: &tokio::runtime::Runtime, cert: &str, key: &str) -> u16 {
    let acceptor = acceptor(cert, key);
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut seen = Vec::new();
                let mut buffer = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buffer[..n]),
                    }
                }
                let events = [
                    r#"{"type":"message_start","message":{"id":"m","model":"tls-model","usage":{"input_tokens":1}}}"#,
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"over tls"}}"#,
                    r#"{"type":"content_block_stop","index":0}"#,
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                    r#"{"type":"message_stop"}"#,
                ];
                let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await;
                for event in events {
                    let frame = format!("data: {event}\n\n");
                    let _ = tls.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
                    let _ = tls.flush().await;
                }
                let _ = tls.write_all(b"0\r\n\r\n").await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

fn payload(url: String) -> Json {
    jobj! {
        "family": "anthropic",
        "baseURL": url,
        "apiKey": "k",
        "modelId": "m",
        "messages": vec![jobj! { "role": "user", "content": "x" }],
        "maxSteps": 1u64,
    }
}

#[test]
fn https_works_for_a_trusted_root_and_refuses_an_untrusted_one() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let port = serve(&runtime, CERT_PEM, KEY_PEM);
    let untrusted_port = serve(&runtime, UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM);
    mewrk_aisdk::http::trust_root_for_tests(der(CERT_PEM));
    let rust = Rust::start();

    let trusted = rust.run("a", &payload(format!("https://127.0.0.1:{port}/v1")));
    assert_eq!(trusted.text(), "over tls");
    assert_eq!(trusted.result().str_at("model"), Some("tls-model"));
    let by_name = rust.run("b", &payload(format!("https://localhost:{port}/v1")));
    assert_eq!(by_name.text(), "over tls");

    // Node words this `Cannot connect to API: self-signed certificate`, and retries are the host's call.
    let refused = rust.run("c", &payload(format!("https://127.0.0.1:{untrusted_port}/v1")));
    let error = refused.error();
    assert_eq!(error.str_at("kind"), Some("transient"), "{error}");
    let message = error.str_at("message").unwrap();
    assert!(message.starts_with("Cannot connect to API") && message.contains("certificate"), "{error}");
    runtime.shutdown_background();
}

/// An empty 200 to every bodiless request, keeping the connection for the next. Counts connections.
fn serve_keep_alive(runtime: &tokio::runtime::Runtime, cert: &str, key: &str) -> (u16, Arc<AtomicUsize>) {
    let acceptor = acceptor(cert, key);
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            counted.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut seen = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    match tls.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buffer[..n]),
                    }
                    if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        seen.clear();
                        let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                        let _ = tls.flush().await;
                    }
                }
            });
        }
    });
    (port, connections)
}

/// `new https.Agent({ ca })`, which GDCH's `ca_cert_path` asks for: those roots and no others, and a pooled connection
/// made under one trust does not serve a request under another.
#[test]
fn a_request_with_its_own_roots_trusts_only_those() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let (bundled_port, _) = serve_keep_alive(&runtime, CERT_PEM, KEY_PEM);
    let (private_port, private_connections) = serve_keep_alive(&runtime, UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM);
    mewrk_aisdk::http::trust_root_for_tests(der(CERT_PEM));
    let roots = Arc::new(vec![der(UNTRUSTED_CERT_PEM)]);
    let get = |port: u16, ca_roots: Option<Arc<Vec<Vec<u8>>>>| {
        let request = HttpRequest { method: "GET", url: format!("https://127.0.0.1:{port}/v1"), headers: Headers::new(), body: None, ca_roots, cancel: Cancel::new() };
        async move {
            let mut response = NetFetch.fetch(request).await?;
            let status = response.status;
            response.body.read_text().await?;
            Ok::<u16, AiError>(status)
        }
    };
    let certificate_failure = |outcome: Result<u16, AiError>| outcome.unwrap_err().message().contains("certificate");
    runtime.block_on(async {
        // The same trust reuses the connection.
        assert_eq!(get(private_port, Some(roots.clone())).await.unwrap(), 200);
        assert_eq!(get(private_port, Some(roots.clone())).await.unwrap(), 200);
        assert_eq!(private_connections.load(Ordering::SeqCst), 1);
        // Without the roots the connection in the pool does not answer, and a new one is refused.
        assert!(certificate_failure(get(private_port, None).await));
        // The other way round too: a connection under the bundled roots, then a request with roots of its own.
        assert_eq!(get(bundled_port, None).await.unwrap(), 200);
        assert!(certificate_failure(get(bundled_port, Some(roots.clone())).await));
    });
    runtime.shutdown_background();
}
