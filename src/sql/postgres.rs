//! What Postgres's SQL looks like.

use super::dialect::Syntax;

/// Postgres's [`Syntax`].
pub(super) struct Postgres;

impl Syntax for Postgres {
    const NAME: &'static str = "Postgres";
    const QUOTE: u8 = b'"';
    const BINARY_PREFIX: &'static [u8] = b"'\\x";
    const BACKSLASH_ESCAPES: bool = false;
    // Identity columns declared GENERATED ALWAYS take the copied values too.
    const VALUES_CLAUSE: &'static [u8] = b") OVERRIDING SYSTEM VALUE VALUES\n";
    // A replica's role skips foreign key checks, which a sample's tables can't pass in the order
    // they're written.
    const FILE_HEADER: &'static [u8] = b"SET client_encoding = 'UTF8';\n\
          SET standard_conforming_strings = on;\n\
          SET session_replication_role = replica;\n\n";
    const FOREIGN_KEYS_QUERY: &'static str = "SELECT c.conname, t.relname, a.attname, p.relname, pa.attname \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_class p ON p.oid = c.confrelid \
         CROSS JOIN LATERAL unnest(c.conkey, c.confkey) WITH ORDINALITY AS k(child, parent, position) \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.child \
         JOIN pg_attribute pa ON pa.attrelid = c.confrelid AND pa.attnum = k.parent \
         WHERE c.contype = 'f' AND t.relnamespace = current_schema()::regnamespace \
         AND p.relnamespace = current_schema()::regnamespace \
         ORDER BY t.relname, c.conname, k.position";
    const COLUMNS_QUERY: (&'static [u8], &'static [u8]) = (
        b"SELECT table_name, column_name, data_type, character_maximum_length \
          FROM information_schema.columns \
          WHERE table_schema = current_schema() AND table_name IN (",
        b") ORDER BY table_name, ordinal_position",
    );
    // Postgres's CONCAT skips NULLs; `||` turns them into NULL, as MySQL's CONCAT does.
    const CONCAT: (&'static [u8], &'static [u8], &'static [u8]) = (b"CAST(", b" || ", b" AS text)");

    // Only quotes need escaping: tapas sets standard_conforming_strings, so a backslash is a
    // plain character.
    fn push_text(out: &mut Vec<u8>, bytes: &[u8]) {
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

    fn push_raw_text(out: &mut Vec<u8>, literal: &[u8]) {
        let mut bytes = literal[1..literal.len() - 1].iter();
        while let Some(&byte) = bytes.next() {
            out.push(byte);
            if byte == b'\'' {
                // push_text doubles every quote; skip the second one.
                bytes.next();
            }
        }
    }

    fn file_footer(tables: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        if !tables.is_empty() {
            push_sequence_reset(&mut out, tables);
        }
        out.extend_from_slice(b"\nRESET session_replication_role;\n");
        out
    }

    fn cutoff(amount: u32, unit: &str) -> String {
        format!("CURRENT_DATE - INTERVAL '{amount} {unit}'")
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
        let mut name = Vec::new();
        Postgres::push_identifier(&mut name, table);
        Postgres::push_text(&mut regclasses, &name);
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
