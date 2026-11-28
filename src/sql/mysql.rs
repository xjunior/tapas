//! What MySQL's SQL looks like.

use mysql_async::consts::ColumnType;

use super::dialect::Syntax;
use super::kind::Kind;

/// The binary character set id, used by BLOB, BINARY and VARBINARY columns.
const BINARY_CHARSET: u16 = 63;

/// MySQL's [`Syntax`].
pub(super) struct MySql;

impl Syntax for MySql {
    const NAME: &'static str = "MySQL";
    const QUOTE: u8 = b'`';
    const BINARY_PREFIX: &'static [u8] = b"X'";
    const BACKSLASH_ESCAPES: bool = true;
    const VALUES_CLAUSE: &'static [u8] = b") VALUES\n";
    const FILE_HEADER: &'static [u8] = b"SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n";
    const FOREIGN_KEYS_QUERY: &'static str = "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, \
         REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_SCHEMA = DATABASE() \
         ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION";
    const COLUMNS_QUERY: (&'static [u8], &'static [u8]) = (
        b"SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
          FROM information_schema.COLUMNS \
          WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME IN (",
        b") ORDER BY TABLE_NAME, ORDINAL_POSITION",
    );
    const CONCAT: (&'static [u8], &'static [u8], &'static [u8]) = (b"CONCAT(", b", ", b")");

    fn push_text(out: &mut Vec<u8>, bytes: &[u8]) {
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

    fn push_raw_text(out: &mut Vec<u8>, literal: &[u8]) {
        let mut bytes = literal[1..literal.len() - 1].iter();
        while let Some(&byte) = bytes.next() {
            if byte != b'\\' {
                out.push(byte);
                continue;
            }
            let escaped = bytes
                .next()
                .expect("push_text follows every backslash with the escaped byte");
            out.push(match escaped {
                b'0' => 0,
                b'n' => b'\n',
                b'r' => b'\r',
                b'Z' => 0x1a,
                other => *other,
            });
        }
    }

    fn file_footer(_tables: &[&str]) -> Vec<u8> {
        b"\nSET FOREIGN_KEY_CHECKS=1;\n".to_vec()
    }

    fn cutoff(amount: u32, unit: &str) -> String {
        format!("CURDATE() - INTERVAL {amount} {unit}")
    }
}

/// Picks how to write the values of a MySQL column with this type and character set.
#[must_use]
pub fn column_kind(column_type: ColumnType, charset: u16) -> Kind {
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
