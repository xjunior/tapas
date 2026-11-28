//! The `--since` span: how far back samplers with a `limit.since` reach.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::sql::{self, Dialect};

/// How far back samplers with a `limit.since` reach: `--since '3 months'`, or `--since ever`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Since {
    /// Every row is kept, however old.
    Ever,
    /// Rows from the last `amount` `unit`s are kept.
    Back {
        /// How many units back the span reaches. It's never 0.
        amount: u32,
        /// What the span counts in.
        unit: Unit,
    },
}

/// The unit of a `Since::Back` span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    /// A calendar day.
    Day,
    /// Seven days.
    Week,
    /// A calendar month.
    Month,
    /// A calendar year.
    Year,
}

impl Unit {
    /// Returns the interval unit keyword, which MySQL and Postgres share.
    #[must_use]
    fn sql(self) -> &'static str {
        match self {
            Unit::Day => "DAY",
            Unit::Week => "WEEK",
            Unit::Month => "MONTH",
            Unit::Year => "YEAR",
        }
    }
}

impl Display for Unit {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let name = match self {
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
            Unit::Year => "year",
        };
        f.write_str(name)
    }
}

impl Since {
    /// Returns whether the span leaves out some rows of a sampler with these `columns`, without
    /// building the condition.
    #[must_use]
    pub fn narrows(&self, columns: &[String]) -> bool {
        match self {
            Since::Ever => false,
            Since::Back { .. } => !columns.is_empty(),
        }
    }

    /// Returns the SQL condition for `columns`, or nothing for `Since::Ever`. Rows match when any
    /// of the columns falls on or after midnight of the cutoff day.
    #[must_use]
    pub fn condition(&self, dialect: Dialect, columns: &[String]) -> Option<String> {
        let Since::Back { amount, unit } = self else {
            return None;
        };
        if !self.narrows(columns) {
            return None;
        }
        let cutoff = match dialect {
            Dialect::MySql => format!("CURDATE() - INTERVAL {amount} {}", unit.sql()),
            Dialect::Postgres => format!("CURRENT_DATE - INTERVAL '{amount} {}'", unit.sql()),
        };
        let mut checks: Vec<String> = columns
            .iter()
            .map(|column| format!("{} >= {cutoff}", sql::identifier(dialect, column)))
            .collect();
        if checks.len() == 1 {
            return checks.pop();
        }
        Some(format!("({})", checks.join(" OR ")))
    }
}

impl FromStr for Since {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("ever") {
            return Ok(Since::Ever);
        }

        let invalid = || format!("`{text}` isn't a span like `2 weeks` or `3 months`, or `ever`");
        let digits = text
            .find(|character: char| !character.is_ascii_digit())
            .ok_or_else(invalid)?;
        let amount: u32 = text[..digits].parse().map_err(|_| invalid())?;
        if amount == 0 {
            return Err(invalid());
        }
        let unit = match text[digits..].trim().to_ascii_lowercase().as_str() {
            "day" | "days" => Unit::Day,
            "week" | "weeks" => Unit::Week,
            "month" | "months" => Unit::Month,
            "year" | "years" => Unit::Year,
            _ => return Err(invalid()),
        };
        Ok(Since::Back { amount, unit })
    }
}

impl Display for Since {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Since::Ever => write!(f, "ever"),
            Since::Back { amount, unit } => {
                let plural = if *amount == 1 { "" } else { "s" };
                write!(f, "{amount} {unit}{plural}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spans() {
        let back = |amount, unit| Since::Back { amount, unit };
        assert_eq!("2 weeks".parse::<Since>(), Ok(back(2, Unit::Week)));
        assert_eq!("3months".parse::<Since>(), Ok(back(3, Unit::Month)));
        assert_eq!(" 1 Day ".parse::<Since>(), Ok(back(1, Unit::Day)));
        assert_eq!("10 years".parse::<Since>(), Ok(back(10, Unit::Year)));
        assert_eq!("EVER".parse::<Since>(), Ok(Since::Ever));
    }

    #[test]
    fn rejects_bad_spans() {
        for bad in [
            "",
            "all",
            "weeks",
            "2",
            "0 days",
            "2 fortnights",
            "-1 day",
            "1.5 months",
        ] {
            let parsed: Result<Since, String> = bad.parse();
            assert!(parsed.is_err(), "{bad:?} parsed as {parsed:?}");
        }
    }

    #[test]
    fn displays_spans() -> Result<(), String> {
        let week: Since = "1 week".parse()?;
        assert_eq!(week.to_string(), "1 week");
        let months: Since = "3 months".parse()?;
        assert_eq!(months.to_string(), "3 months");
        assert_eq!(Since::Ever.to_string(), "ever");
        Ok(())
    }

    #[test]
    fn narrows_only_columns_it_has_a_span_for() -> Result<(), String> {
        let since: Since = "2 weeks".parse()?;
        assert!(
            since.narrows(&["created_at".to_string()]),
            "a span narrows a sampler with a limit since"
        );
        assert!(
            !since.narrows(&[]),
            "a sampler without a since keeps every row"
        );
        assert!(
            !Since::Ever.narrows(&["created_at".to_string()]),
            "--since ever keeps every row"
        );
        Ok(())
    }

    #[test]
    fn builds_conditions() -> Result<(), String> {
        let since: Since = "2 weeks".parse()?;
        assert_eq!(
            since
                .condition(Dialect::MySql, &["created_at".to_string()])
                .as_deref(),
            Some("`created_at` >= CURDATE() - INTERVAL 2 WEEK")
        );
        assert_eq!(
            since
                .condition(
                    Dialect::MySql,
                    &["created_at".to_string(), "updated_at".to_string()]
                )
                .as_deref(),
            Some(
                "(`created_at` >= CURDATE() - INTERVAL 2 WEEK OR `updated_at` >= CURDATE() - INTERVAL 2 WEEK)"
            )
        );
        assert_eq!(
            Since::Ever.condition(Dialect::MySql, &["created_at".to_string()]),
            None
        );
        assert_eq!(since.condition(Dialect::MySql, &[]), None);
        Ok(())
    }

    #[test]
    fn builds_postgres_conditions() -> Result<(), String> {
        let since: Since = "3 months".parse()?;
        assert_eq!(
            since
                .condition(
                    Dialect::Postgres,
                    &["created_at".to_string(), "updatedAt".to_string()]
                )
                .as_deref(),
            Some(
                "(\"created_at\" >= CURRENT_DATE - INTERVAL '3 MONTH' \
                 OR \"updatedAt\" >= CURRENT_DATE - INTERVAL '3 MONTH')"
            )
        );
        Ok(())
    }
}
