//! Awkward types and values beyond the main golden test: domains, composite
//! types, BC dates, odd UTC offsets, huge numerics, big bytea, unusual text.
//!
//! The rule being tested is the one in CLAUDE.md: whatever the server sends,
//! the user sees the exact text, and nothing we do not understand is dropped
//! or "fixed up".
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_types_extra -- --ignored

mod common;

use common::{exec, pg16, run};
use rowlark_core::{DriverError, PostgresDriver, Value};

/// The single value a one-cell query returns.
async fn one(driver: &PostgresDriver, sql: &str) -> Value {
    exec(driver, sql).await.scalar().clone()
}

fn unknown(type_name: &str, text: &str) -> Value {
    Value::Unknown {
        type_name: type_name.to_string(),
        text: text.to_string(),
    }
}

// ----------------------------------------------------------------- domains --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_domain_shows_as_its_base_type() {
    let d = pg16().await;
    exec(
        &d,
        "CREATE DOMAIN pg_temp.posint AS int CHECK (VALUE > 0); \
         CREATE DOMAIN pg_temp.price AS numeric(10,2); \
         CREATE DOMAIN pg_temp.shortt AS varchar(5)",
    )
    .await;
    assert_eq!(one(&d, "SELECT 5::pg_temp.posint").await, Value::Int(5));
    // The decimal stays exact, trailing zero included.
    assert_eq!(
        one(&d, "SELECT 1.50::pg_temp.price").await,
        Value::Decimal("1.50".into())
    );
    assert_eq!(
        one(&d, "SELECT 'abc'::pg_temp.shortt").await,
        Value::Text("abc".into())
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn an_array_of_a_domain_keeps_every_elements_exact_text() {
    // Known quirk, pinned on purpose: elements of a domain array come back as
    // `Unknown` carrying the domain's name, not as `Int`. The text is exact,
    // so nothing is lost; the UI shows it the same way.
    let d = pg16().await;
    exec(&d, "CREATE DOMAIN pg_temp.posint AS int CHECK (VALUE > 0)").await;
    assert_eq!(
        one(&d, "SELECT ARRAY[1, 2]::pg_temp.posint[]").await,
        Value::Array(vec![unknown("posint", "1"), unknown("posint", "2")])
    );
}

// --------------------------------------------------------------- composite --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn composite_values_keep_the_servers_exact_quoting() {
    let d = pg16().await;
    exec(&d, "CREATE TYPE pg_temp.pair AS (a int, b text)").await;
    assert_eq!(
        one(&d, "SELECT ROW(1, 'x')::pg_temp.pair").await,
        unknown("pair", "(1,x)")
    );
    // Commas, quotes and NULLs inside the row: the server's quoting survives.
    assert_eq!(
        one(&d, "SELECT ROW(NULL, 'a,b \"c\"')::pg_temp.pair").await,
        unknown("pair", "(,\"a,b \"\"c\"\"\")")
    );
    assert_eq!(
        one(&d, "SELECT ARRAY[ROW(1, 'x')::pg_temp.pair]").await,
        Value::Array(vec![unknown("pair", "(1,x)")])
    );
}

// ------------------------------------------------------------------- dates --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn bc_dates_and_timestamps_keep_their_era() {
    let d = pg16().await;
    assert_eq!(
        one(&d, "SELECT '0044-03-15 BC'::date").await,
        Value::Date("0044-03-15 BC".into())
    );
    assert_eq!(
        one(&d, "SELECT '0044-03-15 10:00:00 BC'::timestamp").await,
        Value::Timestamp {
            iso: "0044-03-15 10:00:00 BC".into(),
            tz: None
        }
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn timestamps_show_in_the_session_zone_even_for_odd_offsets() {
    let d = pg16().await;
    // Nepal is UTC+05:45, a quarter-hour offset.
    exec(&d, "SET TIME ZONE 'Asia/Kathmandu'").await;
    assert_eq!(
        one(&d, "SELECT '2026-01-01 00:00:00+00'::timestamptz").await,
        Value::Timestamp {
            iso: "2026-01-01 05:45:00+05:45".into(),
            tz: None
        }
    );
    // A time with a +14:00 offset (the largest Postgres accepts) is untouched.
    assert_eq!(
        one(&d, "SELECT '12:00:00+14'::timetz").await,
        Value::Time("12:00:00+14".into())
    );
    assert_eq!(
        one(&d, "SELECT '24:00:00'::time").await,
        Value::Time("24:00:00".into())
    );
}

// ----------------------------------------------------------------- numbers --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_thousand_digit_numeric_is_exact() {
    let d = pg16().await;
    let nines = "9".repeat(1000);
    assert_eq!(
        one(&d, "SELECT repeat('9', 1000)::numeric").await,
        Value::Decimal(nines)
    );
    let tiny = format!("0.{}1", "0".repeat(999));
    assert_eq!(
        one(&d, "SELECT ('0.' || repeat('0', 999) || '1')::numeric").await,
        Value::Decimal(tiny)
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn numeric_special_values_stay_text() {
    let d = pg16().await;
    let r = exec(&d, "SELECT 'NaN'::numeric, 1e-10::numeric, '-0'::numeric").await;
    assert_eq!(
        r.rows(),
        vec![&vec![
            Value::Decimal("NaN".into()),
            Value::Decimal("0.0000000001".into()),
            // numeric has no negative zero; the server prints a plain 0.
            Value::Decimal("0".into()),
        ]]
    );
}

// ------------------------------------------------------------ json and text --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn json_text_is_kept_byte_for_byte() {
    let d = pg16().await;
    // `json` (not `jsonb`) keeps duplicate keys and spacing exactly as typed.
    assert_eq!(
        one(&d, "SELECT '{\"a\":1,\"a\":2}'::json").await,
        Value::Json("{\"a\":1,\"a\":2}".into())
    );
    // Escaped unicode, including an emoji written as a surrogate pair, comes
    // back as the real characters in jsonb.
    assert_eq!(
        one(&d, "SELECT '{\"s\":\"\\u00e9\\ud83d\\ude00\"}'::jsonb").await,
        Value::Json("{\"s\": \"é😀\"}".into())
    );
    // A number too big for any float keeps every digit.
    assert_eq!(
        one(&d, "SELECT '{\"n\":12345678901234567890.123456789}'::jsonb").await,
        Value::Json("{\"n\": 12345678901234567890.123456789}".into())
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn combining_marks_and_direction_controls_are_not_normalised() {
    let d = pg16().await;
    // "a" + combining acute, a right-to-left override and a zero-width joiner.
    let text = "a\u{301} \u{202e}b \u{200d}";
    assert_eq!(
        one(&d, "SELECT E'a\\u0301 \\u202Eb \\u200d'").await,
        Value::Text(text.into())
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_null_character_is_refused_by_the_server_not_mangled_by_us() {
    let d = pg16().await;
    match run(&d, "SELECT chr(0)").await.result {
        Err(DriverError::Server { message, .. }) => assert!(
            message.contains("null character"),
            "unexpected message: {message}"
        ),
        other => panic!("expected a server error, got {other:?}"),
    }
    // The connection is still usable.
    assert_eq!(one(&d, "SELECT 1").await, Value::Int(1));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn bytea_of_a_hundred_thousand_bytes_arrives_whole() {
    let d = pg16().await;
    match one(&d, "SELECT decode(repeat('ab', 100000), 'hex')").await {
        Value::Bytes(bytes) => {
            assert_eq!(bytes.len(), 100_000);
            assert!(bytes.iter().all(|b| *b == 0xab));
        }
        other => panic!("expected bytes, got {other:?}"),
    }
}

// ------------------------------------------------------------ other families --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn types_without_a_dedicated_variant_keep_exact_text_and_real_name() {
    let d = pg16().await;
    assert_eq!(
        one(&d, "SELECT '<a>1</a>'::xml").await,
        unknown("xml", "<a>1</a>")
    );
    assert_eq!(
        one(&d, "SELECT to_tsvector('hello world')").await,
        unknown("tsvector", "'hello':1 'world':2")
    );
    assert_eq!(
        one(&d, "SELECT 'pg_class'::regclass").await,
        unknown("regclass", "pg_class")
    );
    // The one-byte internal type "char" is shown with its quotes, as the
    // server names it, and its text is untouched.
    assert_eq!(
        one(&d, "SELECT 'a'::\"char\"").await,
        unknown("\"CHAR\"", "a")
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn arrays_of_unusual_element_types_parse_or_fall_back_safely() {
    let d = pg16().await;
    // Elements of types we understand are converted one by one.
    assert_eq!(
        one(&d, "SELECT ARRAY['\\xdead'::bytea]").await,
        Value::Array(vec![Value::Bytes(vec![0xde, 0xad])])
    );
    assert_eq!(
        one(&d, "SELECT ARRAY['{\"a\": 1}'::jsonb]").await,
        Value::Array(vec![Value::Json("{\"a\": 1}".into())])
    );
    // Elements of types we do not convert stay exact text.
    assert_eq!(
        one(&d, "SELECT ARRAY['1 day'::interval]").await,
        Value::Array(vec![unknown("INTERVAL", "1 day")])
    );
    // An array that does not start at index 1 is not guessed at: the whole
    // value stays as the server's text, bounds included.
    assert_eq!(
        one(&d, "SELECT '[0:2]={1,2,3}'::int[]").await,
        unknown("INT4[]", "[0:2]={1,2,3}")
    );
}
