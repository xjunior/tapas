use std::fmt;
use std::str::FromStr;

/// How far back samplers with a `time_column` reach: `--since '3 months'`, or `--since ever`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    Ever,
    Back { amount: u32, unit: Unit },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    fn sql(self) -> &'static str {
        match self {
            Unit::Day => "DAY",
            Unit::Week => "WEEK",
            Unit::Month => "MONTH",
            Unit::Year => "YEAR",
        }
    }
}

impl Since {
    /// The SQL condition for `columns`, or nothing for `Since::Ever`. Rows match when any of the
    /// columns falls on or after midnight of the cutoff day.
    pub fn condition(&self, columns: &[String]) -> Option<String> {
        let Since::Back { amount, unit } = self else {
            return None;
        };
        if columns.is_empty() {
            return None;
        }
        let cutoff = format!("CURDATE() - INTERVAL {amount} {}", unit.sql());
        let checks: Vec<String> = columns
            .iter()
            .map(|column| format!("{} >= {cutoff}", crate::sql::ident(column)))
            .collect();
        Some(if checks.len() == 1 {
            checks.into_iter().next().unwrap()
        } else {
            format!("({})", checks.join(" OR "))
        })
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
            .find(|c: char| !c.is_ascii_digit())
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

impl fmt::Display for Since {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Since::Ever => write!(f, "ever"),
            Since::Back { amount, unit } => {
                let name = format!("{unit:?}").to_ascii_lowercase();
                let plural = if *amount == 1 { "" } else { "s" };
                write!(f, "{amount} {name}{plural}")
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
        assert_eq!("2 weeks".parse(), Ok(back(2, Unit::Week)));
        assert_eq!("3months".parse(), Ok(back(3, Unit::Month)));
        assert_eq!(" 1 Day ".parse(), Ok(back(1, Unit::Day)));
        assert_eq!("10 years".parse(), Ok(back(10, Unit::Year)));
        assert_eq!("EVER".parse(), Ok(Since::Ever));
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
            assert!(bad.parse::<Since>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn displays_spans() {
        assert_eq!("1 week".parse::<Since>().unwrap().to_string(), "1 week");
        assert_eq!("3 months".parse::<Since>().unwrap().to_string(), "3 months");
        assert_eq!(Since::Ever.to_string(), "ever");
    }

    #[test]
    fn builds_conditions() {
        let since: Since = "2 weeks".parse().unwrap();
        assert_eq!(
            since.condition(&["created_at".into()]).unwrap(),
            "`created_at` >= CURDATE() - INTERVAL 2 WEEK"
        );
        assert_eq!(
            since
                .condition(&["created_at".into(), "updated_at".into()])
                .unwrap(),
            "(`created_at` >= CURDATE() - INTERVAL 2 WEEK OR `updated_at` >= CURDATE() - INTERVAL 2 WEEK)"
        );
        assert_eq!(Since::Ever.condition(&["created_at".into()]), None);
        assert_eq!(since.condition(&[]), None);
    }
}
