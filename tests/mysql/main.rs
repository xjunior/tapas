//! Runs tapas against a real MySQL server. Set TAPAS_TEST_DATABASE_URL to a URL whose user can
//! create databases (no database in the path), e.g. mysql://root:tapas@127.0.0.1:3399
//! Without it the tests skip themselves, unless CI is set, where a missing URL is a failure.

mod composite;
mod cycles;
mod end_to_end;
mod errors;
mod foreign_keys;
mod reach;
mod references;
mod sanitize;
mod support;
mod target;
