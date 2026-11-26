//! Golden type tests for the MySQL and MariaDB driver.
//!
//! One table holds many MySQL types with edge values (min/max ints incl.
//! unsigned, 65-digit decimals, zero dates, `838:59:59`, emoji, binary with
//! zero bytes, NULLs). The test reads every row into `Value`s, writes those
//! values back into a second table as text (the server converts them to the
//! column types), reads the copy, and asserts nothing changed. That proves a
//! value shown in the UI and sent back by an edit comes out exactly as it
//! went in. It runs on MySQL 8, MySQL 9 and MariaDB.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test mysql_golden -- --ignored

mod mysql_support;

use mysql_support::*;
use rowlark_core::{Column, DriverError, Flavor, MySqlDriver, Value};

/// Every column of the golden table. `id` is first so we can order rows.
/// `c_bin_text` is text in a `_bin` collation, which MySQL flags as binary.
const COLUMNS: &str = "\
    id int, c_bool tinyint(1), c_tiny tinyint, c_utiny tinyint unsigned, \
    c_small smallint, c_medium mediumint, c_int int, c_uint int unsigned, \
    c_big bigint, c_ubig bigint unsigned, \
    c_float float, c_double double, c_decimal decimal(65,30), \
    c_char char(5), c_varchar varchar(40), c_tinytext tinytext, c_text text, \
    c_longtext longtext, c_bin_text varchar(20) collate utf8mb4_bin, \
    c_enum enum('sad','ok','happy'), c_set set('a','b','c'), \
    c_binary binary(4), c_varbinary varbinary(8), c_blob blob, c_longblob longblob, \
    c_json json, c_date date, c_time time(6), c_datetime datetime(6), \
    c_timestamp timestamp(6) null, c_year year, c_bit bit(12), c_geom geometry null";

/// Typical values.
const ROW_TYPICAL: &str = "\
    INSERT INTO golden VALUES (1, 1, 7, 200, -300, 70000, 42, 4000000000, \
        1234567890123, 12345678901234567890, 1.5, 0.1, 0.10, \
        'ab', 'hello', 'tiny', 'text', 'long', 'aB', 'ok', 'a,c', \
        X'DEADBEEF', X'00FF00', X'89504E470D0A1A0A', X'0102', \
        '{\"a\": 1}', '2026-11-24', '13:45:10.123456', '2026-11-24 13:45:10.123456', \
        '2026-11-24 13:45:10.123456', 2026, b'101010101010', \
        ST_GeomFromText('POINT(1.5 -2)'))";

/// Edge values: limits, odd text, awkward spacing, the largest dates and times.
const ROW_EDGES: &str = r#"
    INSERT INTO golden VALUES (2, 0, -128, 0, -32768, -8388608, -2147483648, 0,
        -9223372036854775808, 0, -3.40282e38, -1.7976931348623157e308,
        -99999999999999999999999999999999999.999999999999999999999999999999,
        'x', 'line1\nline2 \\ "q" ''s'' 😀 ñ', '', 'trailing spaces   ',
        'e', 'CaseSensitive', 'happy', '',
        X'00000000', X'', X'00FF00', X'FFFFFFFF',
        '[1,   2.0,  {"b":1, "a":2}]', '9999-12-31', '838:59:59.000000',
        '9999-12-31 23:59:59.999999', '2038-01-19 03:14:07.999999', 1901, b'111111111111',
        ST_GeomFromText('POINT(0 0)'))"#;

/// MySQL accepts zero dates unless told not to. They stay exact text.
const ROW_ZERO_DATES: &str = "\
    INSERT INTO golden (id, c_date, c_time, c_datetime, c_timestamp, c_year) VALUES \
        (3, '0000-00-00', '-838:59:59.000000', '0000-00-00 00:00:00', \
         '0000-00-00 00:00:00', 0)";

/// More numeric edges: unsigned maximums, tiny floats, negative zero, tiny decimals.
const ROW_NUMBERS: &str = "\
    INSERT INTO golden (id, c_utiny, c_uint, c_ubig, c_big, c_float, c_double, c_decimal) VALUES \
        (4, 255, 4294967295, 18446744073709551615, 9223372036854775807, \
         1.17549e-38, 2.2250738585072014e-308, 0.000000000000000000000000000001), \
        (5, NULL, NULL, NULL, NULL, 0, -0e0, -0.000000000000000000000000000001)";

/// A row of all NULLs (except the id).
const ROW_NULLS: &str = "INSERT INTO golden (id) VALUES (6)";

struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

async fn query(driver: &MySqlDriver, sql: &str) -> Table {
    let r = exec(driver, sql).await;
    let columns = r
        .columns()
        .first()
        .map(|c| (*c).clone())
        .unwrap_or_default();
    let rows = r.rows().into_iter().cloned().collect();
    Table { columns, rows }
}

/// A SQL string literal. MySQL reads a backslash as an escape, so both
/// backslashes and quotes are doubled, and a zero byte is spelled `\0`.
fn quote(text: &str) -> String {
    let mut out = String::from("'");
    for c in text.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// A value as SQL the server converts back to the column's type. Numbers are
/// written as digits; everything else (dates, decimals, JSON, enums) as text
/// for the server to cast; bytes as hex.
fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        // Exponent form on purpose: MySQL reads a long run of plain digits as
        // an exact DECIMAL, which stops at 65 digits (1.8e308 would become
        // 1e65). `1.8e308` is a DOUBLE. The edit code must do the same.
        Value::Float(f) => format!("{f:e}"),
        Value::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("X'{hex}'")
        }
        Value::Decimal(t)
        | Value::Text(t)
        | Value::Json(t)
        | Value::Date(t)
        | Value::Time(t)
        | Value::Uuid(t) => quote(t),
        Value::Timestamp { iso, .. } => quote(iso),
        Value::Unknown { text, .. } => quote(text),
        Value::Array(_) => panic!("MySQL has no arrays"),
    }
}

/// Create and fill the golden table on a fresh connection to `port`.
async fn golden_driver(port: u16) -> MySqlDriver {
    let driver = connect_to(port).await;
    // Zero dates are refused in the default strict mode; this session allows them.
    exec(&driver, "SET SESSION sql_mode = ''").await;
    exec(
        &driver,
        &format!("CREATE TEMPORARY TABLE golden ({COLUMNS})"),
    )
    .await;
    exec(
        &driver,
        &format!("CREATE TEMPORARY TABLE golden_copy ({COLUMNS})"),
    )
    .await;
    for sql in [
        ROW_TYPICAL,
        ROW_EDGES,
        ROW_ZERO_DATES,
        ROW_NUMBERS,
        ROW_NULLS,
    ] {
        exec(&driver, sql).await;
    }
    driver
}

/// One cell of the golden table, by row id and column name.
fn cell<'a>(table: &'a Table, id: i64, column: &str) -> &'a Value {
    let col = table
        .columns
        .iter()
        .position(|c| c.name == column)
        .unwrap_or_else(|| panic!("no column {column}"));
    let row = table
        .rows
        .iter()
        .find(|r| r[0] == Value::Int(id))
        .unwrap_or_else(|| panic!("no row {id}"));
    &row[col]
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn timestamp(s: &str) -> Value {
    Value::Timestamp {
        iso: s.to_string(),
        tz: None,
    }
}

fn year(s: &str) -> Value {
    Value::Unknown {
        type_name: "YEAR".to_string(),
        text: s.to_string(),
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn every_value_survives_a_read_write_read_round_trip() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let original = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        assert_eq!(original.rows.len(), 6, "{label}");

        // Write every value back as text and read the copy.
        for row in &original.rows {
            let values: Vec<String> = row.iter().map(sql_literal).collect();
            exec(
                &driver,
                &format!("INSERT INTO golden_copy VALUES ({})", values.join(", ")),
            )
            .await;
        }
        let copy = query(&driver, "SELECT * FROM golden_copy ORDER BY id").await;

        for (a, b) in original.rows.iter().zip(&copy.rows) {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_eq!(
                    x, y,
                    "{label}: row {:?}, column {}",
                    a[0], original.columns[i].name
                );
            }
        }
        assert_eq!(original.rows, copy.rows, "{label}");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn typical_values_have_the_expected_variants() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let flavor = driver.ping().await.expect("ping").flavor;
        let t = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        let c = |name: &str| cell(&t, 1, name).clone();

        assert_eq!(c("c_bool"), Value::Bool(true), "{label}");
        assert_eq!(c("c_tiny"), Value::Int(7), "{label}");
        assert_eq!(c("c_utiny"), Value::UInt(200), "{label}");
        assert_eq!(c("c_small"), Value::Int(-300), "{label}");
        assert_eq!(c("c_medium"), Value::Int(70000), "{label}");
        assert_eq!(c("c_int"), Value::Int(42), "{label}");
        assert_eq!(c("c_uint"), Value::UInt(4_000_000_000), "{label}");
        assert_eq!(c("c_big"), Value::Int(1_234_567_890_123), "{label}");
        assert_eq!(
            c("c_ubig"),
            Value::UInt(12_345_678_901_234_567_890),
            "{label}"
        );
        assert_eq!(c("c_float"), Value::Float(1.5), "{label}");
        assert_eq!(c("c_double"), Value::Float(0.1), "{label}");
        // 0.10 stays 0.10 (padded to the column's 30 decimals), never 0.1.
        assert_eq!(
            c("c_decimal"),
            Value::Decimal("0.100000000000000000000000000000".into()),
            "{label}"
        );
        assert_eq!(c("c_char"), text("ab"), "{label}");
        assert_eq!(c("c_varchar"), text("hello"), "{label}");
        assert_eq!(c("c_tinytext"), text("tiny"), "{label}");
        assert_eq!(c("c_text"), text("text"), "{label}");
        assert_eq!(c("c_longtext"), text("long"), "{label}");
        // Text in a `_bin` collation is text, not bytes (see `mysql.rs`).
        assert_eq!(c("c_bin_text"), text("aB"), "{label}");
        assert_eq!(c("c_enum"), text("ok"), "{label}");
        assert_eq!(c("c_set"), text("a,c"), "{label}");
        assert_eq!(
            c("c_binary"),
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            "{label}"
        );
        assert_eq!(
            c("c_varbinary"),
            Value::Bytes(vec![0x00, 0xFF, 0x00]),
            "{label}"
        );
        assert_eq!(
            c("c_blob"),
            Value::Bytes(vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            "{label}"
        );
        assert_eq!(c("c_longblob"), Value::Bytes(vec![1, 2]), "{label}");
        // MySQL has a real JSON type. MariaDB's JSON is LONGTEXT with a check,
        // so there it is plain text, and it keeps the text exactly as typed.
        match flavor {
            Flavor::MariaDb => assert_eq!(c("c_json"), text("{\"a\": 1}"), "{label}"),
            _ => assert_eq!(c("c_json"), Value::Json("{\"a\": 1}".into()), "{label}"),
        }
        assert_eq!(c("c_date"), Value::Date("2026-11-24".into()), "{label}");
        assert_eq!(
            c("c_time"),
            Value::Time("13:45:10.123456".into()),
            "{label}"
        );
        assert_eq!(
            c("c_datetime"),
            timestamp("2026-11-24 13:45:10.123456"),
            "{label}"
        );
        assert_eq!(
            c("c_timestamp"),
            timestamp("2026-11-24 13:45:10.123456"),
            "{label}"
        );
        assert_eq!(c("c_year"), year("2026"), "{label}");
        // BIT(12) holds 12 bits in two bytes.
        assert_eq!(c("c_bit"), Value::Bytes(vec![0x0A, 0xAA]), "{label}");
        // Geometry: 4 bytes of SRID, then the standard binary form of a point.
        match c("c_geom") {
            Value::Bytes(bytes) => assert_eq!(bytes.len(), 25, "{label}"),
            other => panic!("{label}: geometry should be bytes, got {other:?}"),
        }
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn edge_values_stay_exact() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let t = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        let c = |id: i64, name: &str| cell(&t, id, name).clone();

        // Limits of every integer type.
        assert_eq!(c(2, "c_tiny"), Value::Int(-128), "{label}");
        assert_eq!(c(2, "c_small"), Value::Int(-32768), "{label}");
        assert_eq!(c(2, "c_medium"), Value::Int(-8_388_608), "{label}");
        assert_eq!(c(2, "c_int"), Value::Int(-2_147_483_648), "{label}");
        assert_eq!(c(2, "c_big"), Value::Int(i64::MIN), "{label}");
        assert_eq!(c(2, "c_bool"), Value::Bool(false), "{label}");
        assert_eq!(c(4, "c_big"), Value::Int(i64::MAX), "{label}");
        assert_eq!(c(4, "c_utiny"), Value::UInt(255), "{label}");
        assert_eq!(c(4, "c_uint"), Value::UInt(u64::from(u32::MAX)), "{label}");
        assert_eq!(c(4, "c_ubig"), Value::UInt(u64::MAX), "{label}");

        // A 65-digit decimal keeps every digit; so does a tiny one.
        assert_eq!(
            c(2, "c_decimal"),
            Value::Decimal(
                "-99999999999999999999999999999999999.999999999999999999999999999999".into()
            ),
            "{label}"
        );
        assert_eq!(
            c(4, "c_decimal"),
            Value::Decimal("0.000000000000000000000000000001".into()),
            "{label}"
        );
        assert_eq!(
            c(5, "c_decimal"),
            Value::Decimal("-0.000000000000000000000000000001".into()),
            "{label}"
        );

        // Floats are as the server printed them (a FLOAT has about 6 digits).
        assert_eq!(c(2, "c_float"), Value::Float(-3.40282e38), "{label}");
        assert_eq!(
            c(2, "c_double"),
            Value::Float(-1.797_693_134_862_315_7e308),
            "{label}"
        );
        assert_eq!(c(4, "c_float"), Value::Float(1.17549e-38), "{label}");
        assert_eq!(
            c(4, "c_double"),
            Value::Float(2.225_073_858_507_201_4e-308),
            "{label}"
        );

        // Text: quotes, backslashes, a line break, emoji, accents, an empty
        // string (not NULL) and spaces at the end (kept by TEXT).
        assert_eq!(
            c(2, "c_varchar"),
            text("line1\nline2 \\ \"q\" 's' 😀 ñ"),
            "{label}"
        );
        assert_eq!(c(2, "c_tinytext"), text(""), "{label}");
        assert_eq!(c(2, "c_text"), text("trailing spaces   "), "{label}");
        assert_eq!(c(2, "c_bin_text"), text("CaseSensitive"), "{label}");
        assert_eq!(c(2, "c_enum"), text("happy"), "{label}");
        assert_eq!(c(2, "c_set"), text(""), "{label}");

        // Bytes with zero bytes in them stay bytes; an empty blob reads as
        // empty text (the cost of telling text from bytes by value).
        assert_eq!(c(2, "c_binary"), Value::Bytes(vec![0, 0, 0, 0]), "{label}");
        assert_eq!(c(2, "c_varbinary"), text(""), "{label}");
        assert_eq!(
            c(2, "c_blob"),
            Value::Bytes(vec![0x00, 0xFF, 0x00]),
            "{label}"
        );
        assert_eq!(c(2, "c_longblob"), Value::Bytes(vec![0xFF; 4]), "{label}");

        // The widest dates and times MySQL allows.
        assert_eq!(c(2, "c_date"), Value::Date("9999-12-31".into()), "{label}");
        assert_eq!(
            c(2, "c_time"),
            Value::Time("838:59:59.000000".into()),
            "{label}"
        );
        assert_eq!(
            c(2, "c_datetime"),
            timestamp("9999-12-31 23:59:59.999999"),
            "{label}"
        );
        assert_eq!(
            c(2, "c_timestamp"),
            timestamp("2038-01-19 03:14:07.999999"),
            "{label}"
        );
        assert_eq!(c(2, "c_year"), year("1901"), "{label}");
        assert_eq!(c(2, "c_bit"), Value::Bytes(vec![0x0F, 0xFF]), "{label}");

        // Zero dates are kept as typed, not turned into an error or a null.
        assert_eq!(c(3, "c_date"), Value::Date("0000-00-00".into()), "{label}");
        assert_eq!(
            c(3, "c_time"),
            Value::Time("-838:59:59.000000".into()),
            "{label}"
        );
        assert_eq!(
            c(3, "c_datetime"),
            timestamp("0000-00-00 00:00:00.000000"),
            "{label}"
        );
        assert_eq!(
            c(3, "c_timestamp"),
            timestamp("0000-00-00 00:00:00.000000"),
            "{label}"
        );
        assert_eq!(c(3, "c_year"), year("0000"), "{label}");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn json_text_follows_each_servers_own_rules() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let flavor = driver.ping().await.expect("ping").flavor;
        let t = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        // Typed as `[1,   2.0,  {"b":1, "a":2}]`.
        match flavor {
            // MySQL stores JSON in its own binary form and prints it back
            // normalised: keys sorted, one space after `:` and `,`. That is
            // the server's text, and the best we can show.
            Flavor::MySql => assert_eq!(
                cell(&t, 2, "c_json"),
                &Value::Json("[1, 2.0, {\"a\": 2, \"b\": 1}]".into()),
                "{label}"
            ),
            // MariaDB keeps the exact text it was given.
            Flavor::MariaDb => assert_eq!(
                cell(&t, 2, "c_json"),
                &text("[1,   2.0,  {\"b\":1, \"a\":2}]"),
                "{label}"
            ),
            Flavor::Postgres => unreachable!(),
        }
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn null_is_null_and_an_empty_string_is_not() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let t = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        let last = t.rows.last().expect("rows");
        assert_eq!(last[0], Value::Int(6), "{label}");
        for (value, column) in last.iter().zip(&t.columns).skip(1) {
            assert_eq!(value, &Value::Null, "{label}: {}", column.name);
        }
        assert_eq!(cell(&t, 2, "c_tinytext"), &text(""), "{label}");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn column_type_names_are_the_servers_names() {
    for (port, label) in SERVERS {
        let driver = golden_driver(port).await;
        let flavor = driver.ping().await.expect("ping").flavor;
        let t = query(&driver, "SELECT * FROM golden ORDER BY id").await;
        let names: Vec<(&str, &str)> = t
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.type_name.as_str()))
            .collect();
        let expect = |name: &str, type_name: &str| {
            assert!(
                names.contains(&(name, type_name)),
                "{label}: {name} should be {type_name}, columns are {names:?}"
            );
        };
        expect("c_bool", "BOOLEAN");
        expect("c_tiny", "TINYINT");
        expect("c_utiny", "TINYINT UNSIGNED");
        expect("c_medium", "MEDIUMINT");
        expect("c_ubig", "BIGINT UNSIGNED");
        expect("c_decimal", "DECIMAL");
        expect("c_enum", "ENUM");
        expect("c_binary", "BINARY");
        expect("c_varbinary", "VARBINARY");
        expect("c_year", "YEAR");
        expect("c_bit", "BIT");
        expect("c_geom", "GEOMETRY");
        expect("c_datetime", "DATETIME");
        expect("c_timestamp", "TIMESTAMP");
        // The `_bin` text column is reported as VARBINARY by the server's
        // flag; the value is still text (checked elsewhere).
        expect("c_bin_text", "VARBINARY");
        // MariaDB's JSON column is a LONGTEXT; the server calls it BLOB here.
        match flavor {
            Flavor::MariaDb => expect("c_json", "BLOB"),
            _ => expect("c_json", "JSON"),
        }
    }
}

/// KNOWN LIMIT, pinned so a future fix shows up: sqlx 0.8 does not know
/// MySQL 9's VECTOR type, so any result that contains one fails as a whole
/// ("unknown column type 0xf2"), even though the server answered fine.
/// `VECTOR_TO_STRING(column)` works around it. When this test fails because
/// the query now succeeds, the limit is gone: turn it into a type test.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn mysql9_vector_columns_fail_in_sqlx_known_limit() {
    let driver = connect_to(33090).await;
    let r = run(&driver, "SELECT STRING_TO_VECTOR('[1,2]')").await;
    match r.result {
        Err(DriverError::Connection(message)) => {
            assert!(message.contains("unknown column type"), "{message}");
        }
        other => panic!("expected sqlx to refuse the VECTOR column, got {other:?}"),
    }
    // The connection is still usable, and the text form works.
    let r = exec(
        &driver,
        "SELECT VECTOR_TO_STRING(STRING_TO_VECTOR('[1,2]'))",
    )
    .await;
    assert_eq!(r.scalar(), &Value::Text("[1.00000e+00,2.00000e+00]".into()));
}
