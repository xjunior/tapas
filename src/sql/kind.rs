//! How a column's values are written back as SQL.

/// How a column's values are written back as SQL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Values are written as is: `42`, `3.14`.
    Number,
    /// Values are quoted and escaped the way the dialect does.
    Text,
    /// Values are written as the dialect's hex literal.
    Binary,
}
