//! Builds the SQL tapas runs and writes: SELECTs for samplers, literals, and INSERTs.

use mysql_async::consts::ColumnType;

use crate::config::Sampler;

/// The binary character set id, used by BLOB, BINARY and VARBINARY columns.
const BINARY_CHARSET: u16 = 63;

/// The SQL literal for a missing value.
pub const NULL: &[u8] = b"NULL";

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Quotes a table or column name.
#[must_use]
pub fn identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Appends `name` to `out`, quoted as a table or column name.
fn push_identifier(out: &mut Vec<u8>, name: &str) {
    out.push(b'`');
    for &byte in name.as_bytes() {
        if byte == b'`' {
            out.push(b'`');
        }
        out.push(byte);
    }
    out.push(b'`');
}

/// How a column's values are written back as SQL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Values are written as is: `42`, `3.14`.
    Number,
    /// Values are quoted and escaped: `'it\'s'`.
    Text,
    /// Values are written as hex: `X'00ff'`.
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
pub fn literal(kind: Kind, value: Option<&[u8]>) -> Vec<u8> {
    let Some(bytes) = value else {
        return NULL.to_vec();
    };
    let capacity = match kind {
        Kind::Number => bytes.len(),
        Kind::Text => bytes.len() + 2,
        Kind::Binary => bytes.len() * 2 + 3,
    };
    let mut out = Vec::with_capacity(capacity);
    push_literal(&mut out, kind, bytes);
    out
}

/// Appends `bytes`, a value read with the text protocol, to `out` as a SQL literal.
pub fn push_literal(out: &mut Vec<u8>, kind: Kind, bytes: &[u8]) {
    match kind {
        Kind::Number => out.extend_from_slice(bytes),
        Kind::Binary => {
            out.reserve(bytes.len() * 2 + 3);
            out.extend_from_slice(b"X'");
            for &byte in bytes {
                out.push(HEX_DIGITS[usize::from(byte >> 4)]);
                out.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
            }
            out.push(b'\'');
        }
        Kind::Text => {
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

/// Appends the value a non-NULL `literal` of this kind stands for to `out`: the reverse of
/// `push_literal`.
pub fn push_raw(out: &mut Vec<u8>, kind: Kind, literal: &[u8]) {
    match kind {
        Kind::Number => out.extend_from_slice(literal),
        Kind::Binary => {
            // X'…'
            let hex = &literal[2..literal.len() - 1];
            for [high, low] in hex.as_chunks::<2>().0 {
                out.push(hex_value(*high) << 4 | hex_value(*low));
            }
        }
        Kind::Text => {
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
pub fn columns_query(tables: &[&str]) -> Vec<u8> {
    let mut out = b"SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
                    FROM information_schema.COLUMNS \
                    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME IN ("
        .to_vec();
    for (index, table) in tables.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        push_literal(&mut out, Kind::Text, table.as_bytes());
    }
    out.extend_from_slice(b") ORDER BY TABLE_NAME, ORDINAL_POSITION");
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

/// A column of a sampler's table, and the values its parent allows in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Dep {
    /// The column of the sampler's table that points at the parent.
    pub column: String,
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
    sampler: &Sampler,
    time_filter: Option<&str>,
    deps: &[Dep],
    chunk_size: usize,
) -> Vec<Vec<u8>> {
    let mut combinations: Vec<Vec<Vec<u8>>> = vec![Vec::new()];

    for Dep { column, allowed } in deps {
        match allowed {
            Allowed::AnyValue => {
                let mut condition = Vec::with_capacity(column.len() + 14);
                push_identifier(&mut condition, column);
                condition.extend_from_slice(b" IS NOT NULL");
                for conditions in &mut combinations {
                    conditions.push(condition.clone());
                }
            }
            Allowed::Values(values) if values.is_empty() => return Vec::new(),
            Allowed::Values(values) => {
                let chunks: Vec<Vec<u8>> = values
                    .chunks(chunk_size)
                    .map(|chunk| in_list(column, chunk))
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
        .map(|conditions| select(sampler, time_filter, conditions))
        .collect()
}

#[must_use]
fn in_list(column: &str, values: &[Vec<u8>]) -> Vec<u8> {
    let value_bytes: usize = values.iter().map(|value| value.len() + 2).sum();
    let mut out = Vec::with_capacity(column.len() + value_bytes + 8);
    push_identifier(&mut out, column);
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
fn select(sampler: &Sampler, time_filter: Option<&str>, dep_conditions: Vec<Vec<u8>>) -> Vec<u8> {
    let mut conditions: Vec<Vec<u8>> = Vec::new();
    if let Some(filter) = &sampler.filter {
        conditions.push(format!("({filter})").into_bytes());
    }
    if let Some(time_filter) = time_filter {
        conditions.push(time_filter.as_bytes().to_vec());
    }
    conditions.extend(dep_conditions);

    let mut sql = b"SELECT * FROM ".to_vec();
    push_identifier(&mut sql, &sampler.table);
    if !conditions.is_empty() {
        sql.extend_from_slice(b" WHERE ");
        sql.extend_from_slice(&conditions.join(&b" AND "[..]));
    }
    if let Some(order_by) = &sampler.order_by {
        sql.extend_from_slice(format!(" ORDER BY {order_by}").as_bytes());
    }
    sql
}

/// Builds one multi-row INSERT.
#[must_use]
pub fn insert(table: &str, columns: &[String], rows: &[Vec<Vec<u8>>]) -> Vec<u8> {
    // Sized up front: a batch's statement is large, and growing it would copy it several times.
    let header_bytes: usize =
        table.len() + columns.iter().map(|name| name.len() + 4).sum::<usize>();
    let row_bytes: usize = rows
        .iter()
        .map(|row| row.iter().map(|literal| literal.len() + 2).sum::<usize>() + 4)
        .sum();
    let mut out = Vec::with_capacity(header_bytes + row_bytes + 32);
    out.extend_from_slice(b"INSERT INTO ");
    push_identifier(&mut out, table);
    out.extend_from_slice(b" (");
    for (index, name) in columns.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        push_identifier(&mut out, name);
    }
    out.extend_from_slice(b") VALUES\n");
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
            column: column.to_string(),
            allowed,
        }
    }

    fn values(items: &[&str]) -> Allowed {
        Allowed::Values(items.iter().map(|item| item.as_bytes().to_vec()).collect())
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(identifier("users"), "`users`");
        assert_eq!(identifier("we`ird"), "`we``ird`");
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
        assert_eq!(literal(Kind::Text, None), b"NULL");
        assert_eq!(literal(Kind::Number, Some(b"-12.5")), b"-12.5");
        assert_eq!(literal(Kind::Binary, Some(&[0, 0xff, 0x10])), b"X'00ff10'");
        assert_eq!(literal(Kind::Text, Some(b"NULL")), b"'NULL'");
        assert_eq!(
            text(&literal(Kind::Text, Some(b"it's a\\b\n\r\0\x1a")))?,
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
        assert_eq!(text(&literal(Kind::Binary, Some(&bytes)))?, expected);
        Ok(())
    }

    #[test]
    fn quotes_names_in_every_statement() -> Result<(), Utf8Error> {
        let deps = [dep("we`ird", values(&["1"]))];
        let queries = sampler_queries(&sampler("ta`ble", None, None), None, &deps, 10);
        assert_eq!(
            text(&queries[0])?,
            "SELECT * FROM `ta``ble` WHERE `we``ird` IN (1)"
        );
        let columns = ["we`ird".to_string()];
        assert_eq!(
            text(&insert("ta`ble", &columns, &[vec![b"1".to_vec()]]))?,
            "INSERT INTO `ta``ble` (`we``ird`) VALUES\n(1);\n"
        );
        Ok(())
    }

    #[test]
    fn builds_a_plain_select() -> Result<(), Utf8Error> {
        let queries = sampler_queries(&sampler("countries", None, None), None, &[], 10);
        assert_eq!(queries.len(), 1);
        assert_eq!(text(&queries[0])?, "SELECT * FROM `countries`");
        Ok(())
    }

    #[test]
    fn combines_filter_deps_and_order() -> Result<(), Utf8Error> {
        let deps = [dep("country_id", Allowed::AnyValue)];
        let queries = sampler_queries(
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
            text(&sampler_queries(&orders, time_filter, &deps, 10)[0])?,
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
        let queries = sampler_queries(&sampler("phones", None, None), None, &deps, 2);
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
        let queries = sampler_queries(&sampler("phones", None, None), None, &deps, 10);
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
                push_raw(&mut raw, kind, &literal(kind, Some(value)));
                assert_eq!(&raw[4..], value, "{kind:?} {value:?}");
                assert_eq!(&raw[..4], b"kept", "push_raw appends");
            }
        }
        let mut raw = Vec::new();
        push_raw(&mut raw, Kind::Number, b"-12.5");
        assert_eq!(raw, b"-12.5");
    }

    #[test]
    fn asks_for_the_columns_of_tables() -> Result<(), Utf8Error> {
        assert_eq!(
            text(&columns_query(&["users", "it's"]))?,
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
            text(&insert("users", &columns, &rows))?,
            "INSERT INTO `users` (`id`, `name`) VALUES\n(1, 'a'),\n(2, NULL);\n"
        );
        Ok(())
    }
}
