//! How to reach a database: host, login, TLS, timeouts, and `postgres://` URLs.
//!
//! The password is deliberately NOT part of `ConnectionConfig`. It lives in
//! the OS keychain and is passed to `connect` separately, so it cannot end up
//! in a log line (through `{:?}` printing) or in a saved file by accident.

use std::path::PathBuf;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use url::Url;

use crate::driver::DriverError;

/// How to use TLS (encryption) on the connection. These follow the names that
/// `psql` and most database tools use for `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Never encrypt. Fine for local and Docker databases.
    Disable,
    /// Encrypt if the server supports it, otherwise quietly continue without.
    /// This does NOT protect against an attacker who strips the encryption.
    Prefer,
    /// Always encrypt, but accept any certificate (protects against
    /// eavesdropping, not against an impostor server).
    Require,
    /// Always encrypt and check that a trusted authority signed the server's
    /// certificate. In theory this skips the check of the server's name.
    ///
    /// Known limitation: sqlx 0.8.6 only skips the name check for one kind of
    /// "wrong name" error, and the TLS library we get reports IP-address
    /// mismatches as another kind. So today this still rejects a certificate
    /// that is not valid for the host name used. That is stricter, never
    /// weaker, than the label promises. Use `VerifyFull` for remote servers.
    VerifyCa,
    /// Like `VerifyCa`, and also check that the certificate is for the host
    /// we are connecting to. This is the safe choice for remote databases.
    VerifyFull,
}

/// TLS settings. The file paths are PEM files the user picked.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub mode: TlsMode,
    /// A custom certificate authority to trust (for example a cloud
    /// provider's CA bundle). Without it the operating system's trusted
    /// authorities are used.
    pub ca_cert: Option<PathBuf>,
    /// Client certificate and key, for servers that require them.
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

impl TlsConfig {
    /// TLS settings with just a mode and no custom files.
    pub fn new(mode: TlsMode) -> Self {
        Self {
            mode,
            ca_cert: None,
            client_cert: None,
            client_key: None,
        }
    }
}

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub tls: TlsConfig,
    /// How long to wait for the server before giving up on connecting.
    pub connect_timeout: Duration,
    /// Shown to database admins (for example in `pg_stat_activity`), so they
    /// can tell which connections are Rowlark's.
    pub application_name: String,
    /// Ask the server to send TCP keepalive probes on this connection. This
    /// keeps idle connections alive through NATs and load balancers and lets
    /// the server notice when we have disappeared.
    pub keepalive: bool,
}

impl ConnectionConfig {
    /// A plain, unencrypted connection with sensible defaults, which suits
    /// local and Docker databases. Change `tls` for anything remote.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            database: database.into(),
            tls: TlsConfig::new(TlsMode::Disable),
            connect_timeout: Duration::from_secs(10),
            application_name: "Rowlark".to_string(),
            keepalive: true,
        }
    }

    /// Read a `postgres://user:password@host:port/database?sslmode=...` URL.
    ///
    /// Returns the config and the password separately (see the note at the top
    /// of this file). Like `psql`, a missing `sslmode` means `prefer`, a
    /// missing port means 5432, and a missing database means the user name.
    /// Options we do not understand are an error, not silently ignored, so a
    /// typo like `sslmod=require` cannot quietly weaken security.
    pub fn from_postgres_url(url: &str) -> Result<(Self, Option<String>), DriverError> {
        let bad = |message: &str| DriverError::InvalidConfig(message.to_string());

        let parsed = Url::parse(url).map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
            return Err(bad("the URL must start with postgres:// or postgresql://"));
        }
        let host = parsed
            .host_str()
            // IPv6 hosts come with square brackets, which the driver does not want.
            .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
            .filter(|h| !h.is_empty())
            .ok_or_else(|| bad("the URL has no host (Unix sockets are not supported yet)"))?;
        // libpq writes a socket directory as a percent-encoded host
        // (`postgres://u@%2Fvar%2Frun%2Fpostgresql/db`). Without this check it
        // would be tried as a host name and fail later with a confusing DNS error.
        if percent_decode_str(host)
            .decode_utf8_lossy()
            .starts_with('/')
        {
            return Err(bad(
                "Unix sockets are not supported yet; use a host name or IP address",
            ));
        }
        let decode = |text: &str| -> Result<String, DriverError> {
            percent_decode_str(text)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| bad("the URL contains text that is not valid UTF-8"))
        };
        let user = decode(parsed.username())?;
        if user.is_empty() {
            return Err(bad("the URL has no user name"));
        }
        let password = parsed.password().map(decode).transpose()?;
        let database = decode(parsed.path().trim_start_matches('/'))?;
        let database = if database.is_empty() {
            user.clone()
        } else {
            database
        };

        let mut config = Self::new(host, parsed.port().unwrap_or(5432), user, database);
        config.tls = TlsConfig::new(TlsMode::Prefer);
        // Each option may appear once. If one is repeated, one of the copies
        // is not what the user meant, and quietly keeping the last could drop
        // `sslmode=require`, so it is an error (the same idea as unknown options).
        let mut seen: Vec<String> = Vec::new();
        for (key, value) in parsed.query_pairs() {
            if seen.iter().any(|earlier| earlier == key.as_ref()) {
                return Err(DriverError::InvalidConfig(format!(
                    "the URL option \"{key}\" is given twice"
                )));
            }
            seen.push(key.to_string());
            match key.as_ref() {
                "sslmode" => {
                    config.tls.mode = match value.as_ref() {
                        "disable" => TlsMode::Disable,
                        "prefer" => TlsMode::Prefer,
                        "require" => TlsMode::Require,
                        "verify-ca" => TlsMode::VerifyCa,
                        "verify-full" => TlsMode::VerifyFull,
                        other => {
                            return Err(DriverError::InvalidConfig(format!(
                                "sslmode={other} is not supported (use disable, prefer, \
                                 require, verify-ca or verify-full)"
                            )));
                        }
                    }
                }
                "sslrootcert" => config.tls.ca_cert = Some(PathBuf::from(value.as_ref())),
                "sslcert" => config.tls.client_cert = Some(PathBuf::from(value.as_ref())),
                "sslkey" => config.tls.client_key = Some(PathBuf::from(value.as_ref())),
                "application_name" => config.application_name = value.into_owned(),
                "connect_timeout" => {
                    let seconds: u64 = value
                        .parse()
                        .map_err(|_| bad("connect_timeout must be a whole number of seconds"))?;
                    // Zero would mean "give up immediately", so every connection
                    // would fail. (libpq reads 0 as "wait forever"; we have no
                    // such mode, so it is better to say so than to guess.)
                    if seconds == 0 {
                        return Err(bad("connect_timeout must be at least 1 second"));
                    }
                    config.connect_timeout = Duration::from_secs(seconds);
                }
                other => {
                    return Err(DriverError::InvalidConfig(format!(
                        "the URL option \"{other}\" is not supported"
                    )));
                }
            }
        }
        Ok((config, password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> (ConnectionConfig, Option<String>) {
        ConnectionConfig::from_postgres_url(url).expect("valid url")
    }

    fn parse_error(url: &str) -> String {
        match ConnectionConfig::from_postgres_url(url) {
            Err(DriverError::InvalidConfig(message)) => message,
            other => panic!("expected InvalidConfig for {url}, got {other:?}"),
        }
    }

    #[test]
    fn new_gives_a_plain_local_connection_with_sensible_defaults() {
        let config = ConnectionConfig::new("localhost", 5432, "u", "d");
        assert_eq!(config.tls.mode, TlsMode::Disable);
        assert!(config.tls.ca_cert.is_none() && config.tls.client_cert.is_none());
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.application_name, "Rowlark");
        assert!(config.keepalive);
        // The password is not part of the config, so printing it cannot leak one.
        assert!(!format!("{config:?}").to_lowercase().contains("password"));
    }

    #[test]
    fn a_full_url_is_split_into_its_parts() {
        let (config, password) = parse(
            "postgres://alice:s3cret@db.example.com:6543/shop?sslmode=verify-full\
             &sslrootcert=/certs/ca.pem&application_name=Report&connect_timeout=5",
        );
        assert_eq!(config.host, "db.example.com");
        assert_eq!(config.port, 6543);
        assert_eq!(config.user, "alice");
        assert_eq!(config.database, "shop");
        assert_eq!(password.as_deref(), Some("s3cret"));
        assert_eq!(config.tls.mode, TlsMode::VerifyFull);
        assert_eq!(config.tls.ca_cert, Some(PathBuf::from("/certs/ca.pem")));
        assert_eq!(config.application_name, "Report");
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn defaults_follow_psql() {
        let (config, password) = parse("postgresql://bob@localhost");
        assert_eq!(config.port, 5432);
        assert_eq!(config.database, "bob", "database defaults to the user");
        assert_eq!(config.tls.mode, TlsMode::Prefer);
        assert_eq!(config.application_name, "Rowlark");
        assert_eq!(password, None);
    }

    #[test]
    fn percent_encoded_text_is_decoded() {
        let (config, password) = parse("postgres://a%40b:p%3Aw%2Fd%20x@h/my%20db");
        assert_eq!(config.user, "a@b");
        assert_eq!(password.as_deref(), Some("p:w/d x"));
        assert_eq!(config.database, "my db");
    }

    #[test]
    fn ipv6_hosts_lose_their_brackets() {
        let (config, _) = parse("postgres://u@[::1]:5433/d");
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 5433);
    }

    #[test]
    fn client_certificate_options_are_read() {
        let (config, _) = parse("postgres://u@h/d?sslcert=/c/client.crt&sslkey=/c/client.key");
        assert_eq!(config.tls.client_cert, Some(PathBuf::from("/c/client.crt")));
        assert_eq!(config.tls.client_key, Some(PathBuf::from("/c/client.key")));
    }

    #[test]
    fn text_that_is_not_utf8_is_an_error_not_garbage() {
        // %FF is not valid UTF-8, so the user name cannot be read.
        assert!(parse_error("postgres://bad%FFname@h/d").contains("UTF-8"));
    }

    #[test]
    fn every_sslmode_is_understood() {
        for (text, mode) in [
            ("disable", TlsMode::Disable),
            ("prefer", TlsMode::Prefer),
            ("require", TlsMode::Require),
            ("verify-ca", TlsMode::VerifyCa),
            ("verify-full", TlsMode::VerifyFull),
        ] {
            let (config, _) = parse(&format!("postgres://u@h/d?sslmode={text}"));
            assert_eq!(config.tls.mode, mode, "{text}");
        }
    }

    #[test]
    fn mistakes_are_errors_not_silently_ignored() {
        assert!(parse_error("mysql://u@h/d").contains("postgres://"));
        assert!(parse_error("postgres://u@/d").contains("host"));
        assert!(parse_error("postgres:///d").contains("host"));
        assert!(parse_error("postgres://h/d").contains("no user"));
        assert!(parse_error("postgres://u@h/d?sslmode=allow").contains("not supported"));
        // A typo must not quietly drop the security setting.
        assert!(parse_error("postgres://u@h/d?sslmod=require").contains("sslmod"));
        assert!(parse_error("postgres://u@h/d?connect_timeout=soon").contains("seconds"));
        assert!(!parse_error("not a url at all").is_empty());
    }

    #[test]
    fn an_empty_password_counts_as_no_password() {
        // `user:@host` and `user@host` both come back as "no password in the
        // URL", so the caller falls back to the keychain (or an empty login).
        assert_eq!(parse("postgres://u:@h/d").1, None);
        assert_eq!(parse("postgres://u@h/d").1, None);
    }

    #[test]
    fn a_missing_database_means_the_user_name_even_with_a_trailing_slash() {
        assert_eq!(parse("postgres://bob@h/").0.database, "bob");
        assert_eq!(parse("postgres://bob@h").0.database, "bob");
        // A trailing question mark with no options is harmless.
        assert_eq!(parse("postgres://bob@h/shop?").0.database, "shop");
    }

    #[test]
    fn plus_signs_are_literal_in_passwords_but_spaces_in_options() {
        // In the user-info part `+` is just a plus; only `%2B` is spelled out.
        assert_eq!(parse("postgres://u:p+w@h/d").1.as_deref(), Some("p+w"));
        assert_eq!(parse("postgres://u:p%2Bw@h/d").1.as_deref(), Some("p+w"));
        // In the query part, `+` means a space (the usual form encoding).
        let (config, _) = parse("postgres://u@h/d?application_name=My+Report");
        assert_eq!(config.application_name, "My Report");
    }

    #[test]
    fn an_unencoded_at_sign_in_the_password_still_splits_at_the_last_one() {
        let (config, password) = parse("postgres://u:p@ss@h/d");
        assert_eq!(config.user, "u");
        assert_eq!(password.as_deref(), Some("p@ss"));
        assert_eq!(config.host, "h");
    }

    #[test]
    fn ipv6_hosts_work_without_a_port_and_the_scheme_is_case_blind() {
        let (config, _) = parse("POSTGRESQL://u@[2001:db8::1]/d");
        assert_eq!(config.host, "2001:db8::1");
        assert_eq!(config.port, 5432);
    }

    #[test]
    fn whitespace_from_copy_and_paste_is_ignored() {
        let (config, _) = parse("  postgres://u@h/d\n");
        assert_eq!((config.host.as_str(), config.database.as_str()), ("h", "d"));
    }

    #[test]
    fn bad_ports_are_errors() {
        assert!(!parse_error("postgres://u@h:99999/d").is_empty());
        assert!(!parse_error("postgres://u@h:abc/d").is_empty());
    }

    #[test]
    fn an_option_given_twice_is_an_error_not_a_silent_winner() {
        // If the same setting appears twice, one of them is not what the user
        // meant. Picking one quietly could drop `sslmode=require`.
        assert!(parse_error("postgres://u@h/d?sslmode=require&sslmode=disable").contains("twice"));
        assert!(
            parse_error("postgres://u@h/d?application_name=a&application_name=b").contains("twice")
        );
    }

    #[test]
    fn connect_timeout_must_be_a_positive_whole_number_of_seconds() {
        // 0 would make every connection fail at once ("wait zero seconds").
        assert!(parse_error("postgres://u@h/d?connect_timeout=0").contains("at least 1"));
        assert!(parse_error("postgres://u@h/d?connect_timeout=-5").contains("seconds"));
        assert!(parse_error("postgres://u@h/d?connect_timeout=1.5").contains("seconds"));
        assert!(
            parse_error("postgres://u@h/d?connect_timeout=99999999999999999999")
                .contains("seconds")
        );
        assert_eq!(
            parse("postgres://u@h/d?connect_timeout=1")
                .0
                .connect_timeout,
            Duration::from_secs(1)
        );
    }

    #[test]
    fn unix_socket_addresses_are_refused_clearly() {
        // A socket directory written the way libpq URLs do it, percent-encoded
        // in the host position, or given as an option.
        for url in [
            "postgres://u@%2Fvar%2Frun%2Fpostgresql/d",
            "postgres://u@/d?host=/var/run/postgresql",
            "postgres://u@h/d?host=/tmp",
        ] {
            assert!(
                parse_error(url).to_lowercase().contains("unix socket")
                    || parse_error(url).contains("host"),
                "{url}"
            );
        }
        // The first form must say so by name, not fail later as an unknown host.
        assert!(
            parse_error("postgres://u@%2Fvar%2Frun%2Fpostgresql/d")
                .contains("Unix sockets are not supported")
        );
    }
}
