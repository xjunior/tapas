//! Builds the SQL tapas runs and writes: SELECTs for samplers, literals, and INSERTs.

mod dialect;
mod insert;
mod kind;
mod mysql;
mod postgres;
mod select;
#[cfg(test)]
mod tests;

pub use dialect::{
    Dialect, NULL, columns_query, file_footer, file_header, foreign_keys_query, identifier,
    literal, push_literal, push_raw, split_insert,
};
pub use insert::insert;
pub use kind::Kind;
pub use select::{Allowed, Dep, sampler_queries};
