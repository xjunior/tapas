use mysql_async::consts::ColumnType;

use crate::config::Sampler;

/// The binary character set id, used by BLOB, BINARY and VARBINARY columns.
const BINARY_CHARSET: u16 = 63;

/// Quotes a table or column name.
pub fn ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// How a column's values are written back as SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Written as is: `42`, `3.14`.
    Number,
    /// Quoted and escaped: `'it\'s'`.
    Text,
    /// Hex: `X'00ff'`.
    Binary,
}

impl Kind {
    pub fn of(column_type: ColumnType, charset: u16) -> Self {
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
            _ if charset == BINARY_CHARSET => Kind::Binary,
            _ => Kind::Text,
        }
    }
}

pub const NULL: &[u8] = b"NULL";

/// Writes a value read with the text protocol as a SQL literal.
pub fn literal(kind: Kind, value: Option<&[u8]>) -> Vec<u8> {
    let Some(bytes) = value else {
        return NULL.to_vec();
    };
    match kind {
        Kind::Number => bytes.to_vec(),
        Kind::Binary => {
            let mut out = Vec::with_capacity(bytes.len() * 2 + 3);
            out.extend_from_slice(b"X'");
            for byte in bytes {
                out.extend_from_slice(format!("{byte:02x}").as_bytes());
            }
            out.push(b'\'');
            out
        }
        Kind::Text => {
            let mut out = Vec::with_capacity(bytes.len() + 2);
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
            out
        }
    }
}

/// The rows a dependency allows.
#[derive(Debug, Clone)]
pub enum Allowed {
    /// The parent kept every row, so any non-NULL value is fine.
    AnyValue,
    /// Literals of the values the parent extracted.
    Values(Vec<Vec<u8>>),
}

/// The SELECTs that run one sampler, without LIMIT. Values lists longer than `chunk_size` are
/// split, and every combination of chunks becomes its own query, so no row matches two queries.
pub fn sampler_queries(
    sampler: &Sampler,
    deps: &[(String, Allowed)],
    chunk_size: usize,
) -> Vec<Vec<u8>> {
    let mut combinations: Vec<Vec<Vec<u8>>> = vec![Vec::new()];

    for (column, allowed) in deps {
        match allowed {
            Allowed::AnyValue => {
                let condition = format!("{} IS NOT NULL", ident(column)).into_bytes();
                combinations
                    .iter_mut()
                    .for_each(|conditions| conditions.push(condition.clone()));
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
        .map(|conditions| select(sampler, conditions))
        .collect()
}

fn in_list(column: &str, values: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("{} IN (", ident(column)).into_bytes();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(value);
    }
    out.push(b')');
    out
}

fn select(sampler: &Sampler, deps: Vec<Vec<u8>>) -> Vec<u8> {
    let mut conditions: Vec<Vec<u8>> = Vec::new();
    if let Some(filter) = &sampler.filter {
        conditions.push(format!("({filter})").into_bytes());
    }
    if let Some(time_filter) = &sampler.time_filter {
        conditions.push(time_filter.clone().into_bytes());
    }
    conditions.extend(deps);

    let mut sql = format!("SELECT * FROM {}", ident(&sampler.table)).into_bytes();
    if !conditions.is_empty() {
        sql.extend_from_slice(b" WHERE ");
        sql.extend_from_slice(&conditions.join(&b" AND "[..]));
    }
    if let Some(order_by) = &sampler.order_by {
        sql.extend_from_slice(format!(" ORDER BY {order_by}").as_bytes());
    }
    sql
}

/// One multi-row INSERT.
pub fn insert(table: &str, columns: &[String], rows: &[Vec<Vec<u8>>]) -> Vec<u8> {
    let names: Vec<String> = columns.iter().map(|name| ident(name)).collect();
    let mut out = format!(
        "INSERT INTO {} ({}) VALUES\n",
        ident(table),
        names.join(", ")
    )
    .into_bytes();
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
    use std::collections::BTreeMap;

    use super::*;

    fn sampler(table: &str, filter: Option<&str>, order_by: Option<&str>) -> Sampler {
        Sampler {
            table: table.into(),
            name: None,
            filter: filter.map(Into::into),
            order_by: order_by.map(Into::into),
            limit: None,
            of: BTreeMap::new(),
            deps: BTreeMap::new(),
            time_column: Vec::new(),
            origin: String::new(),
            time_filter: None,
        }
    }

    fn text(sql: &[u8]) -> &str {
        std::str::from_utf8(sql).unwrap()
    }

    fn values(items: &[&str]) -> Allowed {
        Allowed::Values(items.iter().map(|item| item.as_bytes().to_vec()).collect())
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(ident("users"), "`users`");
        assert_eq!(ident("we`ird"), "`we``ird`");
    }

    #[test]
    fn picks_kinds() {
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_LONGLONG, 63), Kind::Number);
        assert_eq!(
            Kind::of(ColumnType::MYSQL_TYPE_NEWDECIMAL, 63),
            Kind::Number
        );
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_DATETIME, 63), Kind::Text);
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_JSON, 63), Kind::Text);
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_VAR_STRING, 255), Kind::Text);
        assert_eq!(
            Kind::of(ColumnType::MYSQL_TYPE_VAR_STRING, 63),
            Kind::Binary
        );
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_BLOB, 63), Kind::Binary);
        assert_eq!(Kind::of(ColumnType::MYSQL_TYPE_BIT, 63), Kind::Binary);
    }

    #[test]
    fn writes_literals() {
        assert_eq!(literal(Kind::Text, None), b"NULL");
        assert_eq!(literal(Kind::Number, Some(b"-12.5")), b"-12.5");
        assert_eq!(literal(Kind::Binary, Some(&[0, 0xff, 0x10])), b"X'00ff10'");
        assert_eq!(literal(Kind::Text, Some(b"NULL")), b"'NULL'");
        assert_eq!(
            text(&literal(Kind::Text, Some(b"it's a\\b\n\r\0\x1a"))),
            r"'it\'s a\\b\n\r\0\Z'"
        );
    }

    #[test]
    fn builds_a_plain_select() {
        let queries = sampler_queries(&sampler("countries", None, None), &[], 10);
        assert_eq!(queries.len(), 1);
        assert_eq!(text(&queries[0]), "SELECT * FROM `countries`");
    }

    #[test]
    fn combines_filter_deps_and_order() {
        let deps = vec![("country_id".to_string(), Allowed::AnyValue)];
        let queries = sampler_queries(
            &sampler("users", Some("a = 1 OR b = 2"), Some("id DESC")),
            &deps,
            10,
        );
        assert_eq!(
            text(&queries[0]),
            "SELECT * FROM `users` WHERE (a = 1 OR b = 2) AND `country_id` IS NOT NULL ORDER BY id DESC"
        );
    }

    #[test]
    fn adds_the_time_filter_after_the_filter() {
        let mut orders = sampler("orders", Some("total > 0"), None);
        orders.time_filter = Some("`created_at` >= CURDATE() - INTERVAL 2 WEEK".into());
        let deps = vec![("user_id".to_string(), values(&["1"]))];
        assert_eq!(
            text(&sampler_queries(&orders, &deps, 10)[0]),
            "SELECT * FROM `orders` WHERE (total > 0) AND `created_at` >= CURDATE() - INTERVAL 2 WEEK \
             AND `user_id` IN (1)"
        );
    }

    #[test]
    fn chunks_values_into_every_combination() {
        let deps = vec![
            ("user_id".to_string(), values(&["1", "2", "3"])),
            ("number_id".to_string(), values(&["7", "8"])),
        ];
        let queries: Vec<String> = sampler_queries(&sampler("phones", None, None), &deps, 2)
            .iter()
            .map(|query| text(query).to_string())
            .collect();

        assert_eq!(
            queries,
            [
                "SELECT * FROM `phones` WHERE `user_id` IN (1, 2) AND `number_id` IN (7, 8)",
                "SELECT * FROM `phones` WHERE `user_id` IN (3) AND `number_id` IN (7, 8)",
            ]
        );
    }

    #[test]
    fn skips_samplers_whose_parents_extracted_nothing() {
        let deps = vec![("user_id".to_string(), values(&[]))];
        assert!(sampler_queries(&sampler("phones", None, None), &deps, 10).is_empty());
    }

    #[test]
    fn builds_inserts() {
        let rows = vec![
            vec![b"1".to_vec(), b"'a'".to_vec()],
            vec![b"2".to_vec(), b"NULL".to_vec()],
        ];
        assert_eq!(
            text(&insert("users", &["id".into(), "name".into()], &rows)),
            "INSERT INTO `users` (`id`, `name`) VALUES\n(1, 'a'),\n(2, NULL);\n"
        );
    }
}
