//! The INSERTs that write a sample.

use super::dialect::{self, Dialect};

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
    dialect::push_identifier(&mut out, dialect, table);
    out.extend_from_slice(b" (");
    for (index, name) in columns.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        dialect::push_identifier(&mut out, dialect, name);
    }
    out.extend_from_slice(dialect::values_clause(dialect));
    for (index, row) in rows.iter().enumerate() {
        out.extend_from_slice(if index == 0 { b"(" } else { b",\n(" });
        out.extend_from_slice(&row.join(&b", "[..]));
        out.push(b')');
    }
    out.extend_from_slice(b";\n");
    out
}
