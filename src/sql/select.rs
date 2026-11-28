//! The SELECTs that run a sampler.

use super::dialect::{self, Dialect};
use crate::config::{Columns, Sampler};

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
        match allowed {
            Allowed::AnyValue => {
                let condition = not_null(dialect, columns);
                for conditions in &mut combinations {
                    conditions.push(condition.clone());
                }
            }
            Allowed::Values(values) if values.is_empty() => return Vec::new(),
            Allowed::Values(values) => {
                let chunks: Vec<Vec<u8>> = values
                    .chunks(chunk_size)
                    .map(|chunk| in_list(dialect, columns, chunk))
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

/// Writes `columns` as a SELECT does: a column by itself, or several in parentheses.
fn push_columns(out: &mut Vec<u8>, dialect: Dialect, columns: &Columns) {
    let several = columns.as_slice().len() > 1;
    if several {
        out.push(b'(');
    }
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        dialect::push_identifier(out, dialect, column);
    }
    if several {
        out.push(b')');
    }
}

/// Returns the condition that every one of `columns` is set, since a key with a NULL part points
/// at nothing.
#[must_use]
fn not_null(dialect: Dialect, columns: &Columns) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b" AND ");
        }
        dialect::push_identifier(&mut out, dialect, column);
        out.extend_from_slice(b" IS NOT NULL");
    }
    out
}

/// Returns `columns IN (values)`. A composite key's values are tuples, such as `(1, 2)`.
#[must_use]
fn in_list(dialect: Dialect, columns: &Columns, values: &[Vec<u8>]) -> Vec<u8> {
    let value_bytes: usize = values.iter().map(|value| value.len() + 2).sum();
    let mut out = Vec::with_capacity(columns.as_slice().len() * 16 + value_bytes + 8);
    push_columns(&mut out, dialect, columns);
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
    dialect::push_identifier(&mut sql, dialect, &sampler.table);
    if !conditions.is_empty() {
        sql.extend_from_slice(b" WHERE ");
        sql.extend_from_slice(&conditions.join(&b" AND "[..]));
    }
    if let Some(order_by) = &sampler.order_by {
        sql.extend_from_slice(format!(" ORDER BY {order_by}").as_bytes());
    }
    sql
}
