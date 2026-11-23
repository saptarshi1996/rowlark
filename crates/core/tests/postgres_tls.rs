//! TLS tests: encryption, certificate checks and client-certificate login.
//!
//! Needs the TLS test server and its certificates:
//!
//!     scripts/gen-test-certs.sh
//!     docker compose up -d postgres16tls
//!     cargo test -p rowlark-core --test postgres_tls -- --ignored
//!
//! The server certificate is for the name "localhost" only, which lets us
//! prove that `verify-full` rejects a connection made by IP address.

use std::path::PathBuf;

use rowlark_core::{
    ConnectionConfig, Driver, DriverError, PostgresDriver, QueryEvent, TlsConfig, TlsMode, Value,
};
use tokio::sync::mpsc;

const TLS_PORT: u16 = 54326;
const PLAIN_PORT: u16 = 54316;

fn cert(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.test-certs")
        .join(name);
    assert!(
        path.is_file(),
        "missing {path:?}: run scripts/gen-test-certs.sh"
    );
    path
}

fn tls(mode: TlsMode) -> TlsConfig {
    TlsConfig::new(mode)
}

fn with_ca(mode: TlsMode) -> TlsConfig {
    TlsConfig {
        ca_cert: Some(cert("ca.crt")),
        ..TlsConfig::new(mode)
    }
}

fn config(host: &str, port: u16, tls: TlsConfig) -> ConnectionConfig {
    let mut config = ConnectionConfig::new(host, port, "rowlark", "rowlark_test");
    config.tls = tls;
    config
}

/// Run a query that returns one value and give that value back.
async fn scalar(driver: &PostgresDriver, sql: &str) -> Value {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut first = None;
        while let Some(event) = rx.recv().await {
            if let QueryEvent::Rows(rows) = event {
                first =
                    first.or_else(|| rows.into_iter().next().and_then(|r| r.into_iter().next()));
            }
        }
        first
    };
    let (result, value) = tokio::join!(driver.run_query(sql, tx), collect);
    result.unwrap_or_else(|e| panic!("{sql}: {e}"));
    value.expect("one value")
}

/// True when this connection is encrypted, as the server itself reports it.
async fn is_encrypted(driver: &PostgresDriver) -> bool {
    let value = scalar(
        driver,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await;
    value == Value::Bool(true)
}

async fn connect(config: &ConnectionConfig, password: &str) -> Result<PostgresDriver, DriverError> {
    PostgresDriver::connect(config, password).await
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn require_encrypts_the_connection() {
    let driver = connect(
        &config("127.0.0.1", TLS_PORT, tls(TlsMode::Require)),
        "rowlark",
    )
    .await
    .expect("connect with TLS required");
    assert!(is_encrypted(&driver).await);
    // The introspection connection is encrypted too.
    assert!(driver.list_schemas().await.is_ok());
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn disable_and_prefer_behave_as_named() {
    // Disable: plain, even though the server offers TLS.
    let plain = connect(
        &config("127.0.0.1", TLS_PORT, tls(TlsMode::Disable)),
        "rowlark",
    )
    .await
    .expect("plain connection");
    assert!(!is_encrypted(&plain).await);

    // Prefer: encrypted when the server supports it...
    let preferred = connect(
        &config("127.0.0.1", TLS_PORT, tls(TlsMode::Prefer)),
        "rowlark",
    )
    .await
    .expect("prefer, server has TLS");
    assert!(is_encrypted(&preferred).await);

    // ...and plain when it does not.
    let fallback = connect(
        &config("127.0.0.1", PLAIN_PORT, tls(TlsMode::Prefer)),
        "rowlark",
    )
    .await
    .expect("prefer, server has no TLS");
    assert!(!is_encrypted(&fallback).await);
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn require_fails_when_the_server_has_no_tls() {
    let result = connect(
        &config("127.0.0.1", PLAIN_PORT, tls(TlsMode::Require)),
        "rowlark",
    )
    .await;
    assert!(
        matches!(result, Err(DriverError::Connection(_))),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn verify_ca_needs_a_trusted_authority() {
    // Without our test CA the self-made certificate is not trusted.
    let untrusted = connect(
        &config("localhost", TLS_PORT, tls(TlsMode::VerifyCa)),
        "rowlark",
    )
    .await;
    assert!(matches!(untrusted, Err(DriverError::Connection(_))));

    // With the CA it works.
    let trusted = connect(
        &config("localhost", TLS_PORT, with_ca(TlsMode::VerifyCa)),
        "rowlark",
    )
    .await
    .expect("verify-ca with the CA");
    assert!(is_encrypted(&trusted).await);
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn verify_ca_currently_still_rejects_a_wrong_name() {
    // Documents a known sqlx limitation (see `TlsMode::VerifyCa`): verify-ca is
    // meant to ignore the host name, but today it does not for an IP address.
    // It fails safe (stricter, not weaker). If this test starts failing because
    // the connection now succeeds, sqlx fixed it: update the docs and this test.
    let result = connect(
        &config("127.0.0.1", TLS_PORT, with_ca(TlsMode::VerifyCa)),
        "rowlark",
    )
    .await;
    assert!(
        matches!(result, Err(DriverError::Connection(_))),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn verify_full_also_checks_the_host_name() {
    // The certificate is for "localhost", so that name works...
    let by_name = connect(
        &config("localhost", TLS_PORT, with_ca(TlsMode::VerifyFull)),
        "rowlark",
    )
    .await
    .expect("verify-full by the certificate's name");
    assert!(is_encrypted(&by_name).await);

    // ...and the IP address is refused, even though the CA is trusted.
    let by_ip = connect(
        &config("127.0.0.1", TLS_PORT, with_ca(TlsMode::VerifyFull)),
        "rowlark",
    )
    .await;
    assert!(
        matches!(by_ip, Err(DriverError::Connection(_))),
        "{:?}",
        by_ip.err()
    );
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn client_certificate_login() {
    let mut with_cert = config(
        "localhost",
        TLS_PORT,
        TlsConfig {
            client_cert: Some(cert("client.crt")),
            client_key: Some(cert("client.key")),
            ..with_ca(TlsMode::VerifyFull)
        },
    );
    with_cert.user = "rowlark_cert".into();
    // No password: the certificate is the login.
    let driver = connect(&with_cert, "")
        .await
        .expect("log in with a certificate");
    assert_eq!(
        scalar(&driver, "SELECT current_user::text").await,
        Value::Text("rowlark_cert".into())
    );

    // The same user without the certificate is turned away.
    let mut without_cert = config("localhost", TLS_PORT, with_ca(TlsMode::VerifyFull));
    without_cert.user = "rowlark_cert".into();
    assert!(connect(&without_cert, "anything").await.is_err());
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn a_missing_certificate_file_is_a_clear_error() {
    let missing = TlsConfig {
        ca_cert: Some(PathBuf::from("/no/such/ca.pem")),
        ..tls(TlsMode::VerifyFull)
    };
    match connect(&config("localhost", TLS_PORT, missing), "rowlark").await {
        Err(DriverError::InvalidConfig(message)) => {
            assert!(message.contains("CA certificate") && message.contains("/no/such/ca.pem"));
        }
        other => panic!("expected InvalidConfig, got {:?}", other.err()),
    }
}

#[tokio::test]
#[ignore = "needs docker compose and scripts/gen-test-certs.sh"]
async fn a_url_can_carry_every_tls_setting() {
    let ca = cert("ca.crt");
    let url = format!(
        "postgres://rowlark:rowlark@localhost:{TLS_PORT}/rowlark_test\
         ?sslmode=verify-full&sslrootcert={}",
        ca.display()
    );
    let (config, password) = ConnectionConfig::from_postgres_url(&url).expect("url");
    assert_eq!(config.tls.mode, TlsMode::VerifyFull);
    let driver = connect(&config, password.as_deref().unwrap_or(""))
        .await
        .expect("connect from a verify-full URL");
    assert!(is_encrypted(&driver).await);

    // The same URL without the CA cannot be trusted.
    let (untrusted, password) = ConnectionConfig::from_postgres_url(&format!(
        "postgres://rowlark:rowlark@localhost:{TLS_PORT}/rowlark_test?sslmode=verify-full"
    ))
    .expect("url");
    assert!(
        connect(&untrusted, password.as_deref().unwrap_or(""))
            .await
            .is_err()
    );
}
