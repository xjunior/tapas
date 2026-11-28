//! The SQL a database speaks. [`Syntax`] is what each dialect knows about itself; [`Dialect`]
//! picks one at run time.

use std::fmt::{self, Display, Formatter};

use super::kind::Kind;
use super::mysql::MySql;
use super::postgres::Postgres;

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

/// What one dialect knows about its own SQL. A new dialect implements this, and the compiler
/// points to the `match`es on [`Dialect`] that need a new arm.
pub(super) trait Syntax {
    /// The name users know the database by.
    const NAME: &'static str;
    /// The character that quotes a table or column name, doubled inside the name.
    const QUOTE: u8;
    /// What a binary literal starts with, before its hex digits and a closing quote.
    const BINARY_PREFIX: &'static [u8];
    /// Whether a backslash escapes the byte after it inside a text literal.
    const BACKSLASH_ESCAPES: bool;
    /// What follows the column list of an INSERT.
    const VALUES_CLAUSE: &'static [u8];
    /// What a SQL file of the sample starts with: the settings that let its INSERTs load in any
    /// order.
    const FILE_HEADER: &'static [u8];
    /// The query that lists the foreign keys between tables of the current database or schema,
    /// one row per column: constraint, table, column, parent table, parent column. Rows come
    /// ordered by table and constraint, and by the column's position in the key.
    const FOREIGN_KEYS_QUERY: &'static str;
    /// The query that lists the columns of tables in the current database, split around the
    /// list of table names: name, type, and the most characters a value holds.
    const COLUMNS_QUERY: (&'static [u8], &'static [u8]);
    /// Opens, separates and closes the concatenation of a template's pieces. A NULL piece makes
    /// the whole value NULL.
    const CONCAT: (&'static [u8], &'static [u8], &'static [u8]);

    /// Appends `bytes` to `out` as a quoted text literal.
    fn push_text(out: &mut Vec<u8>, bytes: &[u8]);

    /// Appends the value a text `literal` stands for to `out`: the reverse of `push_text`.
    fn push_raw_text(out: &mut Vec<u8>, literal: &[u8]);

    /// What a SQL file of the sample ends with, after the INSERTs into `tables`: the settings put
    /// back, and whatever else the copied rows need.
    fn file_footer(tables: &[&str]) -> Vec<u8>;

    /// The date `amount` `unit`s before today, as an expression.
    fn cutoff(amount: u32, unit: &str) -> String;

    fn push_identifier(out: &mut Vec<u8>, name: &str) {
        out.push(Self::QUOTE);
        for &byte in name.as_bytes() {
            if byte == Self::QUOTE {
                out.push(Self::QUOTE);
            }
            out.push(byte);
        }
        out.push(Self::QUOTE);
    }

    fn push_literal(out: &mut Vec<u8>, kind: Kind, bytes: &[u8]) {
        match kind {
            Kind::Number => out.extend_from_slice(bytes),
            Kind::Binary => {
                out.reserve(Self::BINARY_PREFIX.len() + bytes.len() * 2 + 1);
                out.extend_from_slice(Self::BINARY_PREFIX);
                for &byte in bytes {
                    out.push(HEX_DIGITS[usize::from(byte >> 4)]);
                    out.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
                }
                out.push(b'\'');
            }
            Kind::Text => Self::push_text(out, bytes),
        }
    }

    fn push_raw(out: &mut Vec<u8>, kind: Kind, literal: &[u8]) {
        match kind {
            Kind::Number => out.extend_from_slice(literal),
            Kind::Binary => {
                let hex = &literal[Self::BINARY_PREFIX.len()..literal.len() - 1];
                for [high, low] in hex.as_chunks::<2>().0 {
                    out.push(hex_value(*high) << 4 | hex_value(*low));
                }
            }
            Kind::Text => Self::push_raw_text(out, literal),
        }
    }

    fn columns_query(tables: &[&str]) -> Vec<u8> {
        let (start, end) = Self::COLUMNS_QUERY;
        let mut out = start.to_vec();
        for (index, table) in tables.iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(b", ");
            }
            Self::push_text(&mut out, table.as_bytes());
        }
        out.extend_from_slice(end);
        out
    }

    fn split_insert(statement: &[u8]) -> Vec<Vec<u8>> {
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
                // A doubled quote ends the literal and starts it again, which leaves it open.
                if escaped {
                    escaped = false;
                } else if Self::BACKSLASH_ESCAPES && byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    quoted = false;
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
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => panic!("push_literal writes lowercase hex, not {digit}"),
    }
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

    /// Opens, separates and closes the concatenation of a template's pieces.
    #[must_use]
    pub fn concat(self) -> (&'static [u8], &'static [u8], &'static [u8]) {
        match self {
            Dialect::MySql => MySql::CONCAT,
            Dialect::Postgres => Postgres::CONCAT,
        }
    }

    /// The date `amount` `unit`s before today, as an expression.
    #[must_use]
    pub fn cutoff(self, amount: u32, unit: &str) -> String {
        match self {
            Dialect::MySql => MySql::cutoff(amount, unit),
            Dialect::Postgres => Postgres::cutoff(amount, unit),
        }
    }
}

impl Display for Dialect {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dialect::MySql => MySql::NAME,
            Dialect::Postgres => Postgres::NAME,
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
pub fn push_identifier(out: &mut Vec<u8>, dialect: Dialect, name: &str) {
    match dialect {
        Dialect::MySql => MySql::push_identifier(out, name),
        Dialect::Postgres => Postgres::push_identifier(out, name),
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
    match dialect {
        Dialect::MySql => MySql::push_literal(out, kind, bytes),
        Dialect::Postgres => Postgres::push_literal(out, kind, bytes),
    }
}

/// Appends the value a non-NULL `literal` of this kind stands for to `out`: the reverse of
/// `push_literal`.
pub fn push_raw(out: &mut Vec<u8>, dialect: Dialect, kind: Kind, literal: &[u8]) {
    match dialect {
        Dialect::MySql => MySql::push_raw(out, kind, literal),
        Dialect::Postgres => Postgres::push_raw(out, kind, literal),
    }
}

/// Builds the query that lists the columns of `tables` in the current database: name, type, and
/// the most characters a value holds.
#[must_use]
pub fn columns_query(dialect: Dialect, tables: &[&str]) -> Vec<u8> {
    match dialect {
        Dialect::MySql => MySql::columns_query(tables),
        Dialect::Postgres => Postgres::columns_query(tables),
    }
}

/// Returns the query that lists the foreign keys between tables of the current database or
/// schema, one row per column: constraint, table, column, parent table, parent column. Rows come
/// ordered by table and constraint, and by the column's position in the key.
#[must_use]
pub fn foreign_keys_query(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::MySql => MySql::FOREIGN_KEYS_QUERY,
        Dialect::Postgres => Postgres::FOREIGN_KEYS_QUERY,
    }
}

/// What follows the column list of an INSERT.
pub(super) fn values_clause(dialect: Dialect) -> &'static [u8] {
    match dialect {
        Dialect::MySql => MySql::VALUES_CLAUSE,
        Dialect::Postgres => Postgres::VALUES_CLAUSE,
    }
}

/// Splits an INSERT that `insert` built into one INSERT per row, in order, so the rows can run
/// one at a time.
#[must_use]
pub fn split_insert(dialect: Dialect, statement: &[u8]) -> Vec<Vec<u8>> {
    match dialect {
        Dialect::MySql => MySql::split_insert(statement),
        Dialect::Postgres => Postgres::split_insert(statement),
    }
}

/// What a SQL file of the sample starts with: the settings that let its INSERTs load in any
/// order.
#[must_use]
pub fn file_header(dialect: Dialect) -> &'static [u8] {
    match dialect {
        Dialect::MySql => MySql::FILE_HEADER,
        Dialect::Postgres => Postgres::FILE_HEADER,
    }
}

/// What a SQL file of the sample ends with, after the INSERTs into `tables`: the settings put
/// back and, for Postgres, the tables' sequences moved past the copied rows.
#[must_use]
pub fn file_footer(dialect: Dialect, tables: &[&str]) -> Vec<u8> {
    match dialect {
        Dialect::MySql => MySql::file_footer(tables),
        Dialect::Postgres => Postgres::file_footer(tables),
    }
}
