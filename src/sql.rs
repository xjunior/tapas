//! Builds the SQL tapas runs and writes: SELECTs for samplers, literals, and INSERTs.

use std::fmt::{self, Display, Formatter};

use mysql_async::consts::ColumnType;

use crate::config::{Columns, Sampler};

/// The binary character set id, used by BLOB, BINARY and VARBINARY columns.
const BINARY_CHARSET: u16 = 63;

/// The SQL literal for a missing value.
pub const NULL: &[u8] = b"NULL";

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The SQL a database speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// MySQL: `` `name` ``, `'it\'s'`, `X'00ff'`.
    MySql,
    /// PostgreSQL: `"name"`, `'it''s'`, `'\x00ff'`.
    Postgres,
}

impl Dialect {
    /// Picks the dialect of the database at `url` from its scheme: `mysql://`, or `postgres://`
    /// or `postgresql://`.
    #[must_use]
    pub fn from_url(url: &str) -> Option<Self> {
        let (scheme, _) = url.split_once("://")?;
        match scheme {
            "mysql" => Some(Dialect::MySql),
            "postgres" | "postgresql" => Some(Dialect::Postgres),
            _ => None,
        }
    }
}

impl Display for Dialect {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dialect::MySql => "MySQL",
            Dialect::Postgres => "Postgres",
        })
    }
}

/// Quotes a table or column name.
#[must_use]
pub fn identifier(dialect: Dialect, name: &str) -> String {
    let mut out = Vec::with_capacity(name.len() + 2);
    push_identifier(&mut out, dialect, name);
    String::from_utf8(out).expect("quoting only adds ASCII to a str")
}

/// Appends `name` to `out`, quoted as a table or column name.
fn push_identifier(out: &mut Vec<u8>, dialect: Dialect, name: &str) {
    let quote = match dialect {
        Dialect::MySql => b'`',
        Dialect::Postgres => b'"',
    };
    out.push(quote);
    for &byte in name.as_bytes() {
        if byte == quote {
            out.push(quote);
        }
        out.push(byte);
    }
    out.push(quote);
}

/// How a column's values are written back as SQL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Values are written as is: `42`, `3.14`.
    Number,
    /// Values are quoted and escaped: `'it\'s'`, or `'it''s'` for Postgres.
    Text,
    /// Values are written as hex: `X'00ff'`, or `'\x00ff'` for Postgres.
    Binary,
}

impl Kind {
    /// Picks how to write the values of a column with this type and character set.
    #[must_use]
    pub fn new(column_type: ColumnType, charset: u16) -> Self {
        use ColumnType::*;
        match column_type {
            MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_YEAR => Kind::Number,
            MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY => Kind::Binary,
            // These report the binary charset but are written as text.
            MYSQL_TYPE_DATE
            | MYSQL_TYPE_NEWDATE
            | MYSQL_TYPE_TIME
            | MYSQL_TYPE_TIME2
            | MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2
            | MYSQL_TYPE_JSON => Kind::Text,
            MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_ENUM
            | MYSQL_TYPE_SET
            | MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
            | MYSQL_TYPE_VECTOR
            | MYSQL_TYPE_TYPED_ARRAY
            | MYSQL_TYPE_NULL
            | MYSQL_TYPE_UNKNOWN => {
                if charset == BINARY_CHARSET {
                    Kind::Binary
                } else {
                    Kind::Text
                }
            }
        }
    }
}

/// Writes a value read with the text protocol as a SQL literal.
#[must_use]
pub fn literal(dialect: Dialect, kind: Kind, value: Option<&[u8]>) -> Vec<u8> {
    let Some(bytes) = value else {
        return NULL.to_vec();
    };
    let capacity = match kind {
        Kind::Number => bytes.len(),
        Kind::Text => bytes.len() + 2,
        Kind::Binary => bytes.len() * 2 + 4,
    };
    let mut out = Vec::with_capacity(capacity);
    push_literal(&mut out, dialect, kind, bytes);
    out
}

/// Appends `bytes`, a value read with the text protocol, to `out` as a SQL literal.
pub fn push_literal(out: &mut Vec<u8>, dialect: Dialect, kind: Kind, bytes: &[u8]) {
    match (dialect, kind) {
        (Dialect::MySql | Dialect::Postgres, Kind::Number) => out.extend_from_slice(bytes),
        (Dialect::MySql, Kind::Binary) => push_hex(out, b"X'", bytes),
        (Dialect::Postgres, Kind::Binary) => push_hex(out, b"'\\x", bytes),
        // Only quotes need escaping: tapas sets standard_conforming_strings, so a backslash is
        // a plain character.
        (Dialect::Postgres, Kind::Text) => {
            out.reserve(bytes.len() + 2);
            out.push(b'\'');
            for (index, part) in bytes.split(|&byte| byte == b'\'').enumerate() {
                if index > 0 {
                    out.extend_from_slice(b"''");
                }
                out.extend_from_slice(part);
            }
            out.push(b'\'');
        }
        (Dialect::MySql, Kind::Text) => {
            out.reserve(bytes.len() + 2);
            out.push(b'\'');
            for &byte in bytes {
                match byte {
                    b'\'' => out.extend_from_slice(b"\\'"),
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    0 => out.extend_from_slice(b"\\0"),
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    0x1a => out.extend_from_slice(b"\\Z"),
                    _ => out.push(byte),
                }
            }
            out.push(b'\'');
        }
    }
}

/// Appends `bytes` to `out` as hex digits between `prefix` and a closing quote.
fn push_hex(out: &mut Vec<u8>, prefix: &[u8], bytes: &[u8]) {
    out.reserve(prefix.len() + bytes.len() * 2 + 1);
    out.extend_from_slice(prefix);
    for &byte in bytes {
        out.push(HEX_DIGITS[usize::from(byte >> 4)]);
        out.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
    }
    out.push(b'\'');
}

/// Appends the value a non-NULL `literal` of this kind stands for to `out`: the reverse of
/// `push_literal`.
pub fn push_raw(out: &mut Vec<u8>, dialect: Dialect, kind: Kind, literal: &[u8]) {
    match (dialect, kind) {
        (Dialect::MySql | Dialect::Postgres, Kind::Number) => out.extend_from_slice(literal),
        // X'…'
        (Dialect::MySql, Kind::Binary) => push_unhex(out, &literal[2..literal.len() - 1]),
        // '\x…'
        (Dialect::Postgres, Kind::Binary) => push_unhex(out, &literal[3..literal.len() - 1]),
        (Dialect::Postgres, Kind::Text) => {
            let mut bytes = literal[1..literal.len() - 1].iter();
            while let Some(&byte) = bytes.next() {
                out.push(byte);
                if byte == b'\'' {
                    // push_literal doubles every quote; skip the second one.
                    bytes.next();
                }
            }
        }
        (Dialect::MySql, Kind::Text) => {
            let mut bytes = literal[1..literal.len() - 1].iter();
            while let Some(&byte) = bytes.next() {
                if byte != b'\\' {
                    out.push(byte);
                    continue;
                }
                let escaped = bytes
                    .next()
                    .expect("push_literal follows every backslash with the escaped byte");
                out.push(match escaped {
                    b'0' => 0,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b'Z' => 0x1a,
                    other => *other,
                });
            }
        }
    }
}

fn push_unhex(out: &mut Vec<u8>, hex: &[u8]) {
    for [high, low] in hex.as_chunks::<2>().0 {
        out.push(hex_value(*high) << 4 | hex_value(*low));
    }
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => panic!("push_literal writes lowercase hex, not {digit}"),
    }
}

/// Builds the query that lists the columns of `tables` in the current database: name, type, and
/// the most characters a value holds.
#[must_use]
pub fn columns_query(dialect: Dialect, tables: &[&str]) -> Vec<u8> {
    let (start, end): (&[u8], &[u8]) = match dialect {
        Dialect::MySql => (
            b"SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
              FROM information_schema.COLUMNS \
              WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME IN (",
            b") ORDER BY TABLE_NAME, ORDINAL_POSITION",
        ),
        Dialect::Postgres => (
            b"SELECT table_name, column_name, data_type, character_maximum_length \
              FROM information_schema.columns \
              WHERE table_schema = current_schema() AND table_name IN (",
            b") ORDER BY table_name, ordinal_position",
        ),
    };
    let mut out = start.to_vec();
    for (index, table) in tables.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        push_literal(&mut out, dialect, Kind::Text, table.as_bytes());
    }
    out.extend_from_slice(end);
    out
}

/// The rows a dependency allows.
#[derive(Debug, Clone, PartialEq)]
pub enum Allowed {
    /// The parent kept every row, so any non-NULL value is fine.
    AnyValue,
    /// Only the literals of the values the parent extracted are allowed.
    Values(Vec<Vec<u8>>),
}

/// The columns of a sampler's table that point at a parent, and the values the parent allows in
/// them.
#[derive(Debug, Clone, PartialEq)]
pub struct Dep {
    /// The columns of the sampler's table that point at the parent.
    pub columns: Columns,
    /// The values the column may hold.
    pub allowed: Allowed,
}

/// Builds the SELECTs that run one sampler, without LIMIT. `time_filter` is the plan's
/// `--since` condition, if any. Values lists longer than `chunk_size`
/// are split, and every combination of chunks becomes its own query, so no row matches two
/// queries.
///
/// # Panics
///
/// Panics if `chunk_size` is 0.
#[must_use]
pub fn sampler_queries(
    dialect: Dialect,
    sampler: &Sampler,
    time_filter: Option<&str>,
    deps: &[Dep],
    chunk_size: usize,
) -> Vec<Vec<u8>> {
    let mut combinations: Vec<Vec<Vec<u8>>> = vec![Vec::new()];

    for Dep { columns, allowed } in deps {
        let column = columns
            .single()
            .expect("config only builds single-column references so far");
        match allowed {
            Allowed::AnyValue => {
                let mut condition = Vec::with_capacity(column.len() + 14);
                push_identifier(&mut condition, dialect, column);
                condition.extend_from_slice(b" IS NOT NULL");
                for conditions in &mut combinations {
                    conditions.push(condition.clone());
                }
            }
            Allowed::Values(values) if values.is_empty() => return Vec::new(),
            Allowed::Values(values) => {
                let chunks: Vec<Vec<u8>> = values
                    .chunks(chunk_size)
                    .map(|chunk| in_list(dialect, column, chunk))
                    .collect();
                combinations = combinations
                    .into_iter()
                    .flat_map(|conditions| {
                        chunks.iter().map(move |chunk| {
                            let mut conditions = conditions.clone();
                            conditions.push(chunk.clone());
                            conditions
                        })
                    })
                    .collect();
            }
        }
    }

    combinations
        .into_iter()
        .map(|conditions| select(dialect, sampler, time_filter, conditions))
        .collect()
}

#[must_use]
fn in_list(dialect: Dialect, column: &str, values: &[Vec<u8>]) -> Vec<u8> {
    let value_bytes: usize = values.iter().map(|value| value.len() + 2).sum();
    let mut out = Vec::with_capacity(column.len() + value_bytes + 8);
    push_identifier(&mut out, dialect, column);
    out.extend_from_slice(b" IN (");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(value);
    }
    out.push(b')');
    out
}

#[must_use]
fn select(
    dialect: Dialect,
    sampler: &Sampler,
    time_filter: Option<&str>,
    dep_conditions: Vec<Vec<u8>>,
) -> Vec<u8> {
    let mut conditions: Vec<Vec<u8>> = Vec::new();
    if let Some(filter) = &sampler.filter {
        conditions.push(format!("({filter})").into_bytes());
    }
    if let Some(time_filter) = time_filter {
        conditions.push(time_filter.as_bytes().to_vec());
    }
    conditions.extend(dep_conditions);

    let mut sql = b"SELECT * FROM ".to_vec();
    push_identifier(&mut sql, dialect, &sampler.table);
    if !conditions.is_empty() {
        sql.extend_from_slice(b" WHERE ");
        sql.extend_from_slice(&conditions.join(&b" AND "[..]));
    }
    if let Some(order_by) = &sampler.order_by {
        sql.extend_from_slice(format!(" ORDER BY {order_by}").as_bytes());
    }
    sql
}

/// Splits an INSERT that `insert` built into one INSERT per row, in order, so the rows can run
/// one at a time.
#[must_use]
pub fn split_insert(dialect: Dialect, statement: &[u8]) -> Vec<Vec<u8>> {
    // The first line names the table and columns; each row is a `(...)` after it. Only quoted
    // literals can hold parentheses, so a scan that skips them finds every row.
    let Some(newline) = statement.iter().position(|&byte| byte == b'\n') else {
        return Vec::new();
    };
    let head = &statement[..=newline];
    let mut rows = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut row_start = 0;
    for (index, &byte) in statement.iter().enumerate().skip(head.len()) {
        if quoted {
            match (dialect, byte) {
                _ if escaped => escaped = false,
                (Dialect::MySql, b'\\') => escaped = true,
                // A doubled quote ends the literal and starts it again, which leaves it open.
                (Dialect::MySql | Dialect::Postgres, b'\'') => quoted = false,
                (Dialect::MySql | Dialect::Postgres, _) => {}
            }
            continue;
        }
        match byte {
            b'\'' => quoted = true,
            b'(' => row_start = index,
            b')' => {
                let mut row = Vec::with_capacity(head.len() + index - row_start + 3);
                row.extend_from_slice(head);
                row.extend_from_slice(&statement[row_start..=index]);
                row.extend_from_slice(b";\n");
                rows.push(row);
            }
            _ => {}
        }
    }
    rows
}

/// What a SQL file of the sample starts with: the settings that let its INSERTs load in any
/// order.
#[must_use]
pub fn file_header(dialect: Dialect) -> &'static [u8] {
    match dialect {
        Dialect::MySql => b"SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n",
        // A replica's role skips foreign key checks, which a sample's tables can't pass in the
        // order they're written.
        Dialect::Postgres => {
            b"SET client_encoding = 'UTF8';\n\
              SET standard_conforming_strings = on;\n\
              SET session_replication_role = replica;\n\n"
        }
    }
}

/// What a SQL file of the sample ends with, after the INSERTs into `tables`: the settings put
/// back and, for Postgres, the tables' sequences moved past the copied rows.
#[must_use]
pub fn file_footer(dialect: Dialect, tables: &[&str]) -> Vec<u8> {
    match dialect {
        Dialect::MySql => b"\nSET FOREIGN_KEY_CHECKS=1;\n".to_vec(),
        Dialect::Postgres => {
            let mut out = Vec::new();
            if !tables.is_empty() {
                push_sequence_reset(&mut out, tables);
            }
            out.extend_from_slice(b"\nRESET session_replication_role;\n");
            out
        }
    }
}

/// Appends a Postgres block that moves the sequence behind each `serial` or identity column of
/// `tables` to the column's highest value, so rows the database numbers itself don't clash
/// with copied ones. MySQL needs none: its AUTO_INCREMENT follows the rows inserted.
fn push_sequence_reset(out: &mut Vec<u8>, tables: &[&str]) {
    use std::io::Write;

    let mut regclasses = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        if index > 0 {
            regclasses.extend_from_slice(b", ");
        }
        let name = identifier(Dialect::Postgres, table);
        push_literal(
            &mut regclasses,
            Dialect::Postgres,
            Kind::Text,
            name.as_bytes(),
        );
        regclasses.extend_from_slice(b"::regclass");
    }
    // The block is dollar-quoted, with a tag no table name holds, so none can end it early.
    let mut tag = "$tapas$".to_string();
    let mut attempt = 0;
    while regclasses
        .windows(tag.len())
        .any(|window| window == tag.as_bytes())
    {
        attempt += 1;
        tag = format!("$tapas{attempt}$");
    }

    out.extend_from_slice(format!("\nDO {tag}\n").as_bytes());
    out.extend_from_slice(
        b"DECLARE\n  serial record;\nBEGIN\n  FOR serial IN\n    \
          SELECT attrelid::regclass::text AS tbl, attname AS col,\n           \
          pg_get_serial_sequence(attrelid::regclass::text, attname) AS seq\n    \
          FROM pg_attribute\n    WHERE attrelid IN (",
    );
    out.extend_from_slice(&regclasses);
    // Writing to a `Vec` can't fail.
    let _ = write!(
        out,
        ") AND attnum > 0 AND NOT attisdropped\n  LOOP\n    \
         IF serial.seq IS NOT NULL THEN\n      \
         EXECUTE format('SELECT setval(%L, max(%I)) FROM %s HAVING max(%I) IS NOT NULL',\n        \
         serial.seq, serial.col, serial.tbl, serial.col);\n    \
         END IF;\n  END LOOP;\nEND\n{tag};\n"
    );
}

/// Builds one multi-row INSERT.
#[must_use]
pub fn insert(dialect: Dialect, table: &str, columns: &[String], rows: &[Vec<Vec<u8>>]) -> Vec<u8> {
    // Sized up front: a batch's statement is large, and growing it would copy it several times.
    let header_bytes: usize =
        table.len() + columns.iter().map(|name| name.len() + 4).sum::<usize>();
    let row_bytes: usize = rows
        .iter()
        .map(|row| row.iter().map(|literal| literal.len() + 2).sum::<usize>() + 4)
        .sum();
    let mut out = Vec::with_capacity(header_bytes + row_bytes + 56);
    out.extend_from_slice(b"INSERT INTO ");
    push_identifier(&mut out, dialect, table);
    out.extend_from_slice(b" (");
    for (index, name) in columns.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        push_identifier(&mut out, dialect, name);
    }
    out.extend_from_slice(match dialect {
        Dialect::MySql => b") VALUES\n",
        // Identity columns declared GENERATED ALWAYS take the copied values too.
        Dialect::Postgres => b") OVERRIDING SYSTEM VALUE VALUES\n",
    });
    for (index, row) in rows.iter().enumerate() {
        out.extend_from_slice(if index == 0 { b"(" } else { b",\n(" });
        out.extend_from_slice(&row.join(&b", "[..]));
        out.push(b')');
    }
    out.extend_from_slice(b";\n");
    out
}

#[cfg(test)]
mod tests {
    use std::str::Utf8Error;

    use super::*;

    fn sampler(table: &str, filter: Option<&str>, order_by: Option<&str>) -> Sampler {
        Sampler {
            table: table.to_string(),
            filter: filter.map(String::from),
            order_by: order_by.map(String::from),
            ..Sampler::default()
        }
    }

    fn text(sql: &[u8]) -> Result<&str, Utf8Error> {
        str::from_utf8(sql)
    }

    fn dep(column: &str, allowed: Allowed) -> Dep {
        Dep {
            columns: Columns::from(column),
            allowed,
        }
    }

    fn values(items: &[&str]) -> Allowed {
        Allowed::Values(items.iter().map(|item| item.as_bytes().to_vec()).collect())
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(identifier(Dialect::MySql, "users"), "`users`");
        assert_eq!(identifier(Dialect::MySql, "we`ird"), "`we``ird`");
    }

    #[test]
    fn picks_kinds() {
        assert_eq!(Kind::new(ColumnType::MYSQL_TYPE_LONGLONG, 63), Kind::Number);
        assert_eq!(
            Kind::new(ColumnType::MYSQL_TYPE_NEWDECIMAL, 63),
            Kind::Number
        );
        assert_eq!(Kind::new(ColumnType::MYSQL_TYPE_DATETIME, 63), Kind::Text);
        assert_eq!(Kind::new(ColumnType::MYSQL_TYPE_JSON, 63), Kind::Text);
        assert_eq!(
            Kind::new(ColumnType::MYSQL_TYPE_VAR_STRING, 255),
            Kind::Text
        );
        assert_eq!(
            Kind::new(ColumnType::MYSQL_TYPE_VAR_STRING, 63),
            Kind::Binary
        );
        assert_eq!(Kind::new(ColumnType::MYSQL_TYPE_BLOB, 63), Kind::Binary);
        assert_eq!(Kind::new(ColumnType::MYSQL_TYPE_BIT, 63), Kind::Binary);
    }

    #[test]
    fn writes_literals() -> Result<(), Utf8Error> {
        assert_eq!(literal(Dialect::MySql, Kind::Text, None), b"NULL");
        assert_eq!(
            literal(Dialect::MySql, Kind::Number, Some(b"-12.5")),
            b"-12.5"
        );
        assert_eq!(
            literal(Dialect::MySql, Kind::Binary, Some(&[0, 0xff, 0x10])),
            b"X'00ff10'"
        );
        assert_eq!(
            literal(Dialect::MySql, Kind::Text, Some(b"NULL")),
            b"'NULL'"
        );
        assert_eq!(
            text(&literal(
                Dialect::MySql,
                Kind::Text,
                Some(b"it's a\\b\n\r\0\x1a")
            ))?,
            r"'it\'s a\\b\n\r\0\Z'"
        );
        Ok(())
    }

    #[test]
    fn writes_every_byte_as_two_hex_digits() -> Result<(), Utf8Error> {
        use std::fmt::Write;

        let bytes: Vec<u8> = (0..=u8::MAX).collect();
        let mut expected = String::from("X'");
        for byte in &bytes {
            write!(expected, "{byte:02x}").expect("writing to a String doesn't fail");
        }
        expected.push('\'');
        assert_eq!(
            text(&literal(Dialect::MySql, Kind::Binary, Some(&bytes)))?,
            expected
        );
        Ok(())
    }

    #[test]
    fn quotes_names_in_every_statement() -> Result<(), Utf8Error> {
        let deps = [dep("we`ird", values(&["1"]))];
        let queries = sampler_queries(
            Dialect::MySql,
            &sampler("ta`ble", None, None),
            None,
            &deps,
            10,
        );
        assert_eq!(
            text(&queries[0])?,
            "SELECT * FROM `ta``ble` WHERE `we``ird` IN (1)"
        );
        let columns = ["we`ird".to_string()];
        assert_eq!(
            text(&insert(
                Dialect::MySql,
                "ta`ble",
                &columns,
                &[vec![b"1".to_vec()]]
            ))?,
            "INSERT INTO `ta``ble` (`we``ird`) VALUES\n(1);\n"
        );
        Ok(())
    }

    #[test]
    fn builds_a_plain_select() -> Result<(), Utf8Error> {
        let queries = sampler_queries(
            Dialect::MySql,
            &sampler("countries", None, None),
            None,
            &[],
            10,
        );
        assert_eq!(queries.len(), 1);
        assert_eq!(text(&queries[0])?, "SELECT * FROM `countries`");
        Ok(())
    }

    #[test]
    fn combines_filter_deps_and_order() -> Result<(), Utf8Error> {
        let deps = [dep("country_id", Allowed::AnyValue)];
        let queries = sampler_queries(
            Dialect::MySql,
            &sampler("users", Some("a = 1 OR b = 2"), Some("id DESC")),
            None,
            &deps,
            10,
        );
        assert_eq!(
            text(&queries[0])?,
            "SELECT * FROM `users` WHERE (a = 1 OR b = 2) AND `country_id` IS NOT NULL ORDER BY id DESC"
        );
        Ok(())
    }

    #[test]
    fn adds_the_time_filter_after_the_filter() -> Result<(), Utf8Error> {
        let orders = sampler("orders", Some("total > 0"), None);
        let time_filter = Some("`created_at` >= CURDATE() - INTERVAL 2 WEEK");
        let deps = [dep("user_id", values(&["1"]))];
        assert_eq!(
            text(&sampler_queries(Dialect::MySql, &orders, time_filter, &deps, 10)[0])?,
            "SELECT * FROM `orders` WHERE (total > 0) AND `created_at` >= CURDATE() - INTERVAL 2 WEEK \
             AND `user_id` IN (1)"
        );
        Ok(())
    }

    #[test]
    fn chunks_values_into_every_combination() -> Result<(), Utf8Error> {
        let deps = [
            dep("user_id", values(&["1", "2", "3"])),
            dep("number_id", values(&["7", "8"])),
        ];
        let queries = sampler_queries(
            Dialect::MySql,
            &sampler("phones", None, None),
            None,
            &deps,
            2,
        );
        let queries: Vec<&str> = queries
            .iter()
            .map(|query| text(query))
            .collect::<Result<_, _>>()?;

        assert_eq!(
            queries,
            [
                "SELECT * FROM `phones` WHERE `user_id` IN (1, 2) AND `number_id` IN (7, 8)",
                "SELECT * FROM `phones` WHERE `user_id` IN (3) AND `number_id` IN (7, 8)",
            ]
        );
        Ok(())
    }

    #[test]
    fn skips_samplers_whose_parents_extracted_nothing() {
        let deps = [dep("user_id", values(&[]))];
        let queries = sampler_queries(
            Dialect::MySql,
            &sampler("phones", None, None),
            None,
            &deps,
            10,
        );
        assert!(
            queries.is_empty(),
            "a parent with no values allows no rows, yet got {} queries",
            queries.len()
        );
    }

    #[test]
    fn reads_literals_back() {
        let values: [&[u8]; 6] = [
            b"",
            b"plain",
            b"it's a\\b\n\r\0\x1a",
            b"\\'",
            &[0, 0xff, 0x10, b'\'', b'\\'],
            "ção".as_bytes(),
        ];
        for kind in [Kind::Text, Kind::Binary] {
            for value in values {
                let mut raw = b"kept".to_vec();
                push_raw(
                    &mut raw,
                    Dialect::MySql,
                    kind,
                    &literal(Dialect::MySql, kind, Some(value)),
                );
                assert_eq!(&raw[4..], value, "{kind:?} {value:?}");
                assert_eq!(&raw[..4], b"kept", "push_raw appends");
            }
        }
        let mut raw = Vec::new();
        push_raw(&mut raw, Dialect::MySql, Kind::Number, b"-12.5");
        assert_eq!(raw, b"-12.5");
    }

    #[test]
    fn asks_for_the_columns_of_tables() -> Result<(), Utf8Error> {
        assert_eq!(
            text(&columns_query(Dialect::MySql, &["users", "it's"]))?,
            "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME IN ('users', 'it\\'s') \
             ORDER BY TABLE_NAME, ORDINAL_POSITION"
        );
        Ok(())
    }

    #[test]
    fn builds_inserts() -> Result<(), Utf8Error> {
        let rows = vec![
            vec![b"1".to_vec(), b"'a'".to_vec()],
            vec![b"2".to_vec(), b"NULL".to_vec()],
        ];
        let columns = ["id".to_string(), "name".to_string()];
        assert_eq!(
            text(&insert(Dialect::MySql, "users", &columns, &rows))?,
            "INSERT INTO `users` (`id`, `name`) VALUES\n(1, 'a'),\n(2, NULL);\n"
        );
        Ok(())
    }

    #[test]
    fn picks_dialects_from_urls() {
        assert_eq!(
            Dialect::from_url("mysql://root@db/shop"),
            Some(Dialect::MySql)
        );
        assert_eq!(
            Dialect::from_url("postgres://db/shop"),
            Some(Dialect::Postgres)
        );
        assert_eq!(
            Dialect::from_url("postgresql://db/shop"),
            Some(Dialect::Postgres)
        );
        assert_eq!(Dialect::from_url("sqlite://shop.db"), None);
        assert_eq!(Dialect::from_url("db/shop"), None);
    }

    #[test]
    fn quotes_postgres_identifiers() {
        assert_eq!(identifier(Dialect::Postgres, "users"), r#""users""#);
        assert_eq!(identifier(Dialect::Postgres, r#"we"ird"#), r#""we""ird""#);
        assert_eq!(identifier(Dialect::Postgres, "we`ird"), r#""we`ird""#);
    }

    #[test]
    fn writes_postgres_literals() -> Result<(), Utf8Error> {
        let postgres = Dialect::Postgres;
        assert_eq!(literal(postgres, Kind::Text, None), b"NULL");
        assert_eq!(literal(postgres, Kind::Number, Some(b"-12.5")), b"-12.5");
        assert_eq!(
            literal(postgres, Kind::Binary, Some(&[0, 0xff, 0x10])),
            br"'\x00ff10'"
        );
        // Backslashes are plain characters with standard_conforming_strings, which tapas sets.
        assert_eq!(
            text(&literal(postgres, Kind::Text, Some(b"it's a\\b\n'")))?,
            "'it''s a\\b\n'''"
        );
        Ok(())
    }

    #[test]
    fn reads_postgres_literals_back() {
        let values: [&[u8]; 5] = [b"", b"plain", b"it's a\\b\n", b"''", "ção".as_bytes()];
        for kind in [Kind::Text, Kind::Binary] {
            for value in values {
                let mut raw = b"kept".to_vec();
                let literal = literal(Dialect::Postgres, kind, Some(value));
                push_raw(&mut raw, Dialect::Postgres, kind, &literal);
                assert_eq!(&raw[4..], value, "{kind:?} {value:?}");
            }
        }
    }

    #[test]
    fn builds_postgres_selects() -> Result<(), Utf8Error> {
        let deps = [
            dep("user_id", values(&["'1'", "'2'"])),
            dep("country_id", Allowed::AnyValue),
        ];
        let queries = sampler_queries(
            Dialect::Postgres,
            &sampler("Phones", Some("a = 1"), Some("id")),
            Some(r#""created_at" >= CURRENT_DATE - INTERVAL '2 WEEK'"#),
            &deps,
            10,
        );
        assert_eq!(
            text(&queries[0])?,
            "SELECT * FROM \"Phones\" WHERE (a = 1) \
             AND \"created_at\" >= CURRENT_DATE - INTERVAL '2 WEEK' \
             AND \"user_id\" IN ('1', '2') AND \"country_id\" IS NOT NULL ORDER BY id"
        );
        Ok(())
    }

    #[test]
    fn builds_postgres_inserts_that_set_identity_columns() -> Result<(), Utf8Error> {
        let rows = vec![
            vec![b"'1'".to_vec(), b"'a'".to_vec()],
            vec![b"'2'".to_vec(), b"NULL".to_vec()],
        ];
        let columns = ["id".to_string(), "Name".to_string()];
        assert_eq!(
            text(&insert(Dialect::Postgres, "users", &columns, &rows))?,
            "INSERT INTO \"users\" (\"id\", \"Name\") OVERRIDING SYSTEM VALUE VALUES\n\
             ('1', 'a'),\n('2', NULL);\n"
        );
        Ok(())
    }

    #[test]
    fn asks_postgres_for_the_columns_of_tables() -> Result<(), Utf8Error> {
        assert_eq!(
            text(&columns_query(Dialect::Postgres, &["users", "it's"]))?,
            "SELECT table_name, column_name, data_type, character_maximum_length \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name IN ('users', 'it''s') \
             ORDER BY table_name, ordinal_position"
        );
        Ok(())
    }

    #[test]
    fn frames_mysql_files() -> Result<(), Utf8Error> {
        assert_eq!(
            text(file_header(Dialect::MySql))?,
            "SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n"
        );
        assert_eq!(
            text(&file_footer(Dialect::MySql, &["users"]))?,
            "\nSET FOREIGN_KEY_CHECKS=1;\n"
        );
        Ok(())
    }

    #[test]
    fn frames_postgres_files() -> Result<(), Utf8Error> {
        assert_eq!(
            text(file_header(Dialect::Postgres))?,
            "SET client_encoding = 'UTF8';\n\
             SET standard_conforming_strings = on;\n\
             SET session_replication_role = replica;\n\n"
        );
        assert_eq!(
            text(&file_footer(Dialect::Postgres, &[]))?,
            "\nRESET session_replication_role;\n"
        );
        let footer = file_footer(Dialect::Postgres, &["users", "it's"]);
        let footer = text(&footer)?;
        assert!(
            footer.contains(r#"attrelid IN ('"users"'::regclass, '"it''s"'::regclass)"#),
            "the footer moves the sequences of the sample's tables only: {footer}"
        );
        assert!(
            footer.contains("setval(") && footer.ends_with("\nRESET session_replication_role;\n"),
            "{footer}"
        );
        Ok(())
    }

    #[test]
    fn postgres_footers_quote_the_block_with_a_tag_no_table_uses() -> Result<(), Utf8Error> {
        let footer = file_footer(Dialect::Postgres, &["a$tapas$b"]);
        let footer = text(&footer)?;
        assert!(
            footer.starts_with("\nDO $tapas1$") && footer.contains("\n$tapas1$;\n"),
            "{footer}"
        );
        Ok(())
    }

    #[test]
    fn splits_an_insert_into_one_per_row() -> Result<(), Utf8Error> {
        let columns = vec!["id".to_string(), "note".to_string()];
        for dialect in [Dialect::MySql, Dialect::Postgres] {
            // Quotes, parentheses, commas and newlines inside values don't end a row.
            let notes: [&[u8]; 4] = [b"it's (a), b", b"line\n(two)", b"x'),\n('y", b"ends in \\"];
            let rows: Vec<Vec<Vec<u8>>> = notes
                .iter()
                .enumerate()
                .map(|(id, note)| {
                    let id = id.to_string().into_bytes();
                    vec![
                        literal(dialect, Kind::Number, Some(&id)),
                        literal(dialect, Kind::Text, Some(note)),
                    ]
                })
                .collect();
            let statement = insert(dialect, "notes", &columns, &rows);
            let split = split_insert(dialect, &statement);
            let expected: Vec<Vec<u8>> = rows
                .iter()
                .map(|row| insert(dialect, "notes", &columns, std::slice::from_ref(row)))
                .collect();
            assert_eq!(
                split.len(),
                expected.len(),
                "{dialect}: one statement per row"
            );
            for (got, want) in split.iter().zip(&expected) {
                assert_eq!(text(got)?, text(want)?, "{dialect}");
            }
        }
        Ok(())
    }
}
