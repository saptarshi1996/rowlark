//! Golden type tests for the Postgres driver.
//!
//! One table holds many Postgres types with edge values (min/max ints, exact
//! decimals, NaN, infinity, emoji, odd JSON spacing, DST timestamps, NULLs).
//! The test reads every row into `Value`s, writes those values back into a
//! second table as text (the server casts them to the column types), reads the
//! copy, and asserts nothing changed. That proves that a value shown in the UI
//! and sent back by an edit comes out exactly as it went in.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_golden -- --ignored

use rowlark_core::{
    Column, ConnectionConfig, Driver, PostgresDriver, QueryEvent, QuerySummary, Value,
};
use tokio::sync::mpsc;

/// The Postgres test servers: (port, label).
const SERVERS: [(u16, &str); 4] = [
    (54314, "pg14"),
    (54315, "pg15"),
    (54316, "pg16"),
    (54317, "pg17"),
];

/// Every column of the golden table. `id` is first so we can order rows.
/// `pg_temp.mood` is a throwaway enum, to check that a type we have never
/// heard of still shows up as text, with its real name, instead of crashing.
const CREATE_TABLE: &str = "\
    CREATE TYPE pg_temp.mood AS ENUM ('sad', 'ok', 'happy'); \
    CREATE TEMP TABLE golden ( \
        id int, c_bool bool, c_int2 int2, c_int4 int4, c_int8 int8, \
        c_float4 float4, c_float8 float8, c_numeric numeric, \
        c_text text, c_varchar varchar(10), c_char char(5), c_bytea bytea, \
        c_json json, c_jsonb jsonb, c_date date, c_time time, c_timetz timetz, \
        c_timestamp timestamp, c_timestamptz timestamptz, c_uuid uuid, \
        c_interval interval, c_int_array int4[], c_text_array text[], \
        c_enum pg_temp.mood, c_inet inet, c_oid oid, \
        c_matrix int4[], c_numeric_array numeric[], c_enum_array pg_temp.mood[], \
        c_money money, c_range int4range, c_bit bit(4), c_macaddr macaddr, c_point point, \
        c_cidr cidr, c_tsrange tstzrange \
    ); \
    CREATE TEMP TABLE golden_copy (LIKE golden)";

/// Typical values.
const ROW_TYPICAL: &str = "\
    INSERT INTO golden VALUES (1, true, 7, 42, 1234567890123, 1.5, 0.1, 0.10, \
        'hello', 'abc', 'ab', '\\xdeadbeef', '{\"a\": 1}', '{\"a\": 1}', \
        '2026-11-24', '13:45:10.123456', '13:45:10+05:30', \
        '2026-11-24 13:45:10.123456', '2026-11-24 13:45:10.123456+00', \
        'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '1 year 2 mons 3 days 04:05:06', \
        '{1,2,3}', '{\"a b\",\"c,d\"}', 'ok', '192.168.0.1/24', 4294967295, \
        '{{1,2},{3,4}}', '{0.10,12345678901234567890.123456789,NaN}', '{sad,happy}', \
        '$1,234.56', '[1,10)', B'1010', '08:00:2b:01:02:03', '(1.5,-2)', \
        '10.1.2.0/24', '[2026-01-01 00:00:00+00,2026-02-01 00:00:00+00)')";

/// Edge values: limits, odd text, awkward spacing, a DST timestamp, infinity.
const ROW_EDGES: &str = "\
    INSERT INTO golden VALUES (2, false, -32768, -2147483648, -9223372036854775808, \
        '-Infinity', 'NaN', '12345678901234567890.123456789012345678901234567890', \
        E'line1\\nline2 \\\\ \"q\" ''s'' 😀 ñ', 'ten chars!', 'x', '\\x00ff00', \
        '[1,   2.0,  {\"b\":1, \"a\":2}]', '{\"b\": 1, \"a\": [1,2,{\"c\":null}]}', \
        'infinity', '24:00:00', '23:59:59.999999-14:00', '-infinity', \
        '2026-03-29 01:30:00+00', 'ffffffff-ffff-ffff-ffff-ffffffffffff', \
        '-1 days -02:00:00', '{NULL,1}', '{NULL,\"NULL\",\"with \\\"quote\\\"\",\"\"}', \
        'happy', '::1', 0, \
        '{{NULL,1},{2,NULL}}', '{}', '{NULL,ok}', \
        '-$0.01', 'empty', B'0000', 'ff:ff:ff:ff:ff:ff', '(0,0)', \
        '::1/128', 'empty')";

/// More numeric edges: NaN decimal, tiny decimal, infinity, negative zero.
const ROW_NUMBERS: &str = "\
    INSERT INTO golden (id, c_float4, c_float8, c_numeric, c_int8) VALUES \
        (3, '-0', 'Infinity', '0.000000000000000000000000000001', 9223372036854775807), \
        (4, 3.4028235e38, 1.7976931348623157e308, 'NaN', 1)";

/// A row of all NULLs (except the id).
const ROW_NULLS: &str = "INSERT INTO golden (id) VALUES (5)";

struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

/// Run one query and collect its columns and rows. (Same idea as the helper
/// in `postgres.rs`: query and collector run side by side.)
async fn query(driver: &PostgresDriver, sql: &str) -> Table {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                QueryEvent::Columns(c) => columns = c,
                QueryEvent::Rows(mut r) => rows.append(&mut r),
                QueryEvent::Notice(_) => {}
            }
        }
        (columns, rows)
    };
    let (summary, (columns, rows)) = tokio::join!(driver.run_query(sql, tx), collect);
    let _: QuerySummary = summary.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"));
    Table { columns, rows }
}

/// A SQL string literal. Doubling single quotes is enough: with
/// `standard_conforming_strings` on (the default) backslashes are plain text.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The text form of a single (non-NULL, non-array) value, as Postgres reads it.
fn text_of(value: &Value) -> String {
    match value {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        // Rust prints NaN as "NaN" and infinity as "inf"; Postgres reads both.
        Value::Float(f) => f.to_string(),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Date(s) | Value::Time(s) | Value::Uuid(s) => s.clone(),
        Value::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            format!("\\x{hex}")
        }
        Value::Timestamp { iso, .. } => iso.clone(),
        Value::Unknown { text, .. } => text.clone(),
        Value::Null | Value::Array(_) => panic!("not a single value: {value:?}"),
    }
}

/// An array as Postgres array text: `{"a","b",NULL,{...}}`. Every element is
/// quoted (with `\` and `"` escaped) so no value can be misread.
fn array_literal(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::Null => "NULL".to_string(),
            Value::Array(inner) => array_literal(inner),
            other => format!(
                "\"{}\"",
                text_of(other).replace('\\', "\\\\").replace('"', "\\\"")
            ),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// Turn a `Value` back into SQL. Everything becomes a quoted text literal and
/// the server converts it to the column's type. This is how edits will work.
fn to_sql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Array(items) => quote(&array_literal(items)),
        other => quote(&text_of(other)),
    }
}

/// Look up one cell by column name.
fn cell<'a>(table: &'a Table, row: usize, column: &str) -> &'a Value {
    let index = table
        .columns
        .iter()
        .position(|c| c.name == column)
        .unwrap_or_else(|| panic!("no column {column}"));
    &table.rows[row][index]
}

async fn golden_round_trip(port: u16, label: &str) {
    let config = ConnectionConfig::new("127.0.0.1", port, "rowlark", "rowlark_test");
    let driver = PostgresDriver::connect(&config, "rowlark")
        .await
        .unwrap_or_else(|e| panic!("{label}: is docker compose up? {e}"));

    query(&driver, CREATE_TABLE).await;
    for insert in [ROW_TYPICAL, ROW_EDGES, ROW_NUMBERS, ROW_NULLS] {
        query(&driver, insert).await;
    }

    // First read.
    let first = query(&driver, "SELECT * FROM golden ORDER BY id").await;
    assert_eq!(first.rows.len(), 5, "{label}");

    // Write every row back as text, then read the copy.
    let values: Vec<String> = first
        .rows
        .iter()
        .map(|row| {
            format!(
                "({})",
                row.iter().map(to_sql).collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    query(
        &driver,
        &format!("INSERT INTO golden_copy VALUES {}", values.join(", ")),
    )
    .await;
    let second = query(&driver, "SELECT * FROM golden_copy ORDER BY id").await;

    // `Float(NaN)` is not equal to itself, so compare the Debug text instead.
    assert_eq!(
        format!("{:?}", first.rows),
        format!("{:?}", second.rows),
        "{label}: values changed after writing back and reading again"
    );

    // Fixed expectations, so a bug that damages both reads the same way is caught.
    let typical = 0;
    assert_eq!(cell(&first, typical, "c_bool"), &Value::Bool(true));
    assert_eq!(
        cell(&first, typical, "c_int8"),
        &Value::Int(1_234_567_890_123)
    );
    assert_eq!(
        cell(&first, typical, "c_numeric"),
        &Value::Decimal("0.10".into())
    );
    assert_eq!(
        cell(&first, typical, "c_char"),
        &Value::Text("ab   ".into())
    );
    assert_eq!(
        cell(&first, typical, "c_bytea"),
        &Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );
    assert_eq!(
        cell(&first, typical, "c_json"),
        &Value::Json("{\"a\": 1}".into())
    );
    assert_eq!(
        cell(&first, typical, "c_date"),
        &Value::Date("2026-11-24".into())
    );
    assert_eq!(
        cell(&first, typical, "c_timestamptz"),
        &Value::Timestamp {
            iso: "2026-11-24 13:45:10.123456+00".into(),
            tz: None
        }
    );
    assert_eq!(cell(&first, typical, "c_oid"), &Value::UInt(4_294_967_295));

    let edges = 1;
    assert_eq!(cell(&first, edges, "c_int8"), &Value::Int(i64::MIN));
    assert_eq!(
        cell(&first, edges, "c_numeric"),
        &Value::Decimal("12345678901234567890.123456789012345678901234567890".into())
    );
    assert_eq!(
        cell(&first, edges, "c_text"),
        &Value::Text("line1\nline2 \\ \"q\" 's' 😀 ñ".into())
    );
    assert_eq!(
        cell(&first, edges, "c_json"),
        &Value::Json("[1,   2.0,  {\"b\":1, \"a\":2}]".into())
    );
    assert_eq!(
        cell(&first, edges, "c_date"),
        &Value::Date("infinity".into())
    );
    assert!(matches!(cell(&first, edges, "c_float8"), Value::Float(f) if f.is_nan()));

    // Types we do not map yet still come through with the server's own text,
    // and their real name, looked up on the introspection connection.
    let mood = |text: &str| Value::Unknown {
        type_name: "mood".into(),
        text: text.into(),
    };
    assert_eq!(cell(&first, edges, "c_enum"), &mood("happy"));
    let type_of = |name: &str| {
        first
            .columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.type_name.as_str())
    };
    assert_eq!(type_of("c_enum"), Some("mood"));
    assert_eq!(type_of("c_enum_array"), Some("mood[]"));
    assert_eq!(type_of("c_int_array"), Some("INT4[]"));
    assert_eq!(
        cell(&first, edges, "c_enum_array"),
        &Value::Array(vec![Value::Null, mood("ok")])
    );
    assert_eq!(
        cell(&first, typical, "c_enum_array"),
        &Value::Array(vec![mood("sad"), mood("happy")])
    );

    // Arrays: parsed into lists, NULL elements kept, quoting undone, nesting kept.
    let ints = |items: &[Option<i64>]| {
        Value::Array(
            items
                .iter()
                .map(|i| i.map_or(Value::Null, Value::Int))
                .collect(),
        )
    };
    assert_eq!(
        cell(&first, typical, "c_int_array"),
        &ints(&[Some(1), Some(2), Some(3)])
    );
    assert_eq!(cell(&first, edges, "c_int_array"), &ints(&[None, Some(1)]));
    assert_eq!(
        cell(&first, typical, "c_text_array"),
        &Value::Array(vec![Value::Text("a b".into()), Value::Text("c,d".into())])
    );
    assert_eq!(
        cell(&first, edges, "c_text_array"),
        &Value::Array(vec![
            Value::Null,
            Value::Text("NULL".into()),
            Value::Text("with \"quote\"".into()),
            Value::Text("".into()),
        ])
    );
    assert_eq!(
        cell(&first, typical, "c_matrix"),
        &Value::Array(vec![ints(&[Some(1), Some(2)]), ints(&[Some(3), Some(4)])])
    );
    assert_eq!(
        cell(&first, edges, "c_matrix"),
        &Value::Array(vec![ints(&[None, Some(1)]), ints(&[Some(2), None])])
    );
    assert_eq!(
        cell(&first, typical, "c_numeric_array"),
        &Value::Array(vec![
            Value::Decimal("0.10".into()),
            Value::Decimal("12345678901234567890.123456789".into()),
            Value::Decimal("NaN".into()),
        ])
    );
    assert_eq!(
        cell(&first, edges, "c_numeric_array"),
        &Value::Array(vec![])
    );

    let numbers = 2;
    assert_eq!(
        cell(&first, numbers, "c_numeric"),
        &Value::Decimal("0.000000000000000000000000000001".into())
    );
    assert_eq!(cell(&first, numbers, "c_int8"), &Value::Int(i64::MAX));

    // The all-NULL row.
    let nulls = 4;
    for column in &first.columns[1..] {
        assert_eq!(
            cell(&first, nulls, &column.name),
            &Value::Null,
            "{}",
            column.name
        );
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn golden_types_round_trip_on_every_postgres_version() {
    for (port, label) in SERVERS {
        golden_round_trip(port, label).await;
    }
}
