//! End-to-end test that `DumpProperties` never leaks a secret, driven through the public surface:
//! spawn a daemon over a temp socket, `Connect` with a password, then `DumpProperties`.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests may unwrap and panic freely")]

use std::path::Path;
use std::time::Duration;

use ironrdp_agent::daemon;
use ironrdp_agent::ipc::{Payload, PropValue, Request, Response};
use ironrdp_agent::transport::{self, Endpoint};
use ironrdp_propertyset::PropertySet;

#[tokio::test]
async fn dump_properties_redacts_password_end_to_end() {
    // A unique temp socket for this test process.
    let socket = std::env::temp_dir().join(format!("ironrdp-agent-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let endpoint = Endpoint(socket.clone());

    // Start the daemon on a background task.
    let daemon_endpoint = endpoint.clone();
    let daemon_task = tokio::spawn(async move { daemon::run(daemon_endpoint).await });

    wait_for_socket(&socket).await;

    // Connect carrying a password. The target is refused immediately, but the live state is seeded
    // synchronously during `Connect`, so the password is present (and must be redacted) on dump.
    let mut properties = PropertySet::new();
    properties.insert("full address", "127.0.0.1:1");
    properties.insert("username", "alice");
    properties.insert("ClearTextPassword", "hunter2");

    let response = transport::send_request(&endpoint, &Request::Connect(properties))
        .await
        .expect("connect request");
    assert_eq!(response, Response::ok(), "connect should be accepted");

    // Dump the live properties and assert the password is redacted.
    let response = transport::send_request(&endpoint, &Request::DumpProperties { filter: None })
        .await
        .expect("dump request");

    let Response::Ok(Payload::Properties(dump)) = response else {
        panic!("expected a properties payload, got {response:?}");
    };

    let password = dump
        .entries
        .iter()
        .find(|entry| entry.key == "ClearTextPassword")
        .expect("password entry present");
    assert_eq!(
        password.value,
        PropValue::Str("<redacted>".to_owned()),
        "password value must be redacted"
    );

    // Belt and suspenders: the secret must not appear anywhere in the dump.
    for entry in &dump.entries {
        if let PropValue::Str(value) = &entry.value {
            assert_ne!(value.as_str(), "hunter2", "secret leaked in key {}", entry.key);
        }
    }

    daemon_task.abort();
    let _ = std::fs::remove_file(&socket);
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket did not appear in time");
}
