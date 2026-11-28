use std::str::Utf8Error;

use mysql_async::consts::ColumnType;

use super::*;
use crate::config::{Columns, Sampler};

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

fn composite(columns: &[&str], allowed: Allowed) -> Dep {
    Dep {
        columns: Columns::new(columns.iter().map(|column| column.to_string()).collect()),
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
fn composite_deps_compare_row_values() -> Result<(), Utf8Error> {
    let sampler = Sampler {
        table: "memberships".to_string(),
        ..Sampler::default()
    };
    let deps = [composite(
        &["tenant_id", "user_id"],
        values(&["(1, 2)", "(1, 3)"]),
    )];
    assert_eq!(
        text(&sampler_queries(Dialect::MySql, &sampler, None, &deps, 10)[0])?,
        "SELECT * FROM `memberships` WHERE (`tenant_id`, `user_id`) IN ((1, 2), (1, 3))"
    );
    assert_eq!(
        text(&sampler_queries(Dialect::Postgres, &sampler, None, &deps, 10)[0])?,
        "SELECT * FROM \"memberships\" WHERE (\"tenant_id\", \"user_id\") IN ((1, 2), (1, 3))"
    );
    assert_eq!(
        sampler_queries(Dialect::MySql, &sampler, None, &deps, 1).len(),
        2,
        "chunks count tuples, not columns"
    );
    Ok(())
}

#[test]
fn composite_deps_that_allow_any_need_every_column() -> Result<(), Utf8Error> {
    let sampler = Sampler {
        table: "memberships".to_string(),
        ..Sampler::default()
    };
    let deps = [
        composite(&["tenant_id", "user_id"], Allowed::AnyValue),
        dep("shop_id", values(&["7"])),
    ];
    assert_eq!(
        text(&sampler_queries(Dialect::MySql, &sampler, None, &deps, 10)[0])?,
        "SELECT * FROM `memberships` WHERE `tenant_id` IS NOT NULL \
         AND `user_id` IS NOT NULL AND `shop_id` IN (7)"
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
fn lists_foreign_keys_of_the_current_database() {
    assert_eq!(
        foreign_keys_query(Dialect::MySql),
        "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, \
         REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_SCHEMA = DATABASE() \
         ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
    );
    assert_eq!(
        foreign_keys_query(Dialect::Postgres),
        "SELECT c.conname, t.relname, a.attname, p.relname, pa.attname \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_class p ON p.oid = c.confrelid \
         CROSS JOIN LATERAL unnest(c.conkey, c.confkey) WITH ORDINALITY AS k(child, parent, position) \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.child \
         JOIN pg_attribute pa ON pa.attrelid = c.confrelid AND pa.attnum = k.parent \
         WHERE c.contype = 'f' AND t.relnamespace = current_schema()::regnamespace \
         AND p.relnamespace = current_schema()::regnamespace \
         ORDER BY t.relname, c.conname, k.position"
    );
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
