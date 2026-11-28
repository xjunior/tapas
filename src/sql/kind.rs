//! How a column's values are written back as SQL.

use mysql_async::consts::ColumnType;

/// The binary character set id, used by BLOB, BINARY and VARBINARY columns.
const BINARY_CHARSET: u16 = 63;

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
