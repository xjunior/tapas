//! Works out from the config which tables to extract, in which order, and what to remember of
//! each table's rows while extracting.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::config::{ColumnRef, Config, Sampler, Sanitize};
use crate::since::Since;
use crate::sql::{self, Allowed, Dep, Dialect};

/// Holds what the extraction needs to know about one table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TablePlan {
    /// The table's samplers, as indexes into `Plan::samplers`.
    pub samplers: Vec<usize>,
    /// Tables whose rows this table's samplers filter on.
    pub parents: BTreeSet<String>,
    /// Tables that filter on this table's rows.
    pub children: BTreeSet<String>,
    /// Columns whose values children filter on, so they're kept while extracting.
    pub keep: BTreeSet<String>,
    /// Some sampler keeps every row.
    pub full: bool,
    /// The same row can come back twice: the table has several samplers, or is in a cycle.
    pub dedupe: bool,
    /// Index into `Plan::cycles` when this table depends on itself, directly or through others.
    pub cycle: Option<usize>,
}

/// Holds every sampler, and what the extraction needs to know to run them in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The configured samplers, followed by the ones `deps` pull in.
    pub samplers: Vec<Sampler>,
    /// Every table some sampler extracts from, by name.
    pub tables: BTreeMap<String, TablePlan>,
    /// Tables in an order where each comes after its parents; tables in a cycle sit together.
    pub order: Vec<String>,
    /// Groups of tables that depend on each other. They're extracted together, in rounds.
    pub cycles: Vec<Vec<String>>,
    /// Index into `samplers` of each named sampler, by name.
    pub named: BTreeMap<String, usize>,
    /// Columns whose values children read from a named sampler's own rows.
    pub named_keep: BTreeMap<String, BTreeSet<String>>,
    /// How far back samplers with a `limit.since` reach, when `--since` was given.
    pub since: Option<Since>,
    /// What to write in place of sensitive values.
    pub sanitize: Sanitize,
}

impl Plan {
    /// Plans the extraction of `config`'s samplers, limited by `since`. Fails when the samplers
    /// contradict each other or depend on tables nothing extracts.
    pub fn try_new(config: Config, since: Option<Since>) -> Result<Self> {
        let Config {
            mut samplers,
            sanitize,
        } = config;
        pull_referenced_rows(&mut samplers, since)?;

        let mut tables: BTreeMap<String, TablePlan> = BTreeMap::new();

        for (index, sampler) in samplers.iter().enumerate() {
            let table = tables.entry(sampler.table.clone()).or_default();
            // A sampler that takes no rows only declares `deps`, so it never runs.
            if sampler.takes_rows() {
                table.samplers.push(index);
            }
            table.full |= keeps_every_row(sampler, since);
        }

        let mut named: BTreeMap<String, usize> = BTreeMap::new();
        for (index, sampler) in samplers.iter().enumerate() {
            let Some(name) = &sampler.name else { continue };
            if tables.contains_key(name) {
                bail!("{}: name `{name}` is also a table", sampler.origin);
            }
            if let Some(&other) = named.get(name) {
                bail!(
                    "{}: name `{name}` is already used by {}",
                    sampler.origin,
                    samplers[other].origin
                );
            }
            named.insert(name.clone(), index);
        }

        let mut named_keep: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for sampler in &samplers {
            for (column, parent) in &sampler.of {
                // A dep names a table, or a named sampler of one.
                let parent_table = match named.get(&parent.table) {
                    Some(&index) => {
                        named_keep
                            .entry(parent.table.clone())
                            .or_default()
                            .insert(parent.column.clone());
                        samplers[index].table.clone()
                    }
                    None => {
                        let Some(parent_plan) = tables.get_mut(&parent.table) else {
                            bail!(
                                "{}: {}.{column} depends on {parent}, but no sampler extracts {}",
                                sampler.origin,
                                sampler.table,
                                parent.table
                            );
                        };
                        parent_plan.keep.insert(parent.column.clone());
                        parent.table.clone()
                    }
                };
                tables
                    .get_mut(&parent_table)
                    .expect("every sampler's table, named or not, has a plan")
                    .children
                    .insert(sampler.table.clone());
                tables
                    .get_mut(&sampler.table)
                    .expect("every sampler's table has a plan")
                    .parents
                    .insert(parent_table);
            }
        }

        for (table, rules) in &sanitize.tables {
            if !tables.contains_key(table)
                && let Some(rule) = rules.first()
            {
                bail!(
                    "{}: sanitize.tables.{table} names a table no sampler extracts",
                    rule.file
                );
            }
        }

        let (order, cycles) = components(&tables);
        for (index, cycle) in cycles.iter().enumerate() {
            for name in cycle {
                tables
                    .get_mut(name)
                    .expect("components only returns planned tables")
                    .cycle = Some(index);
            }
        }
        for table in tables.values_mut() {
            table.dedupe = table.samplers.len() > 1 || table.cycle.is_some();
        }

        let plan = Self {
            samplers,
            tables,
            order,
            cycles,
            named,
            named_keep,
            since,
            sanitize,
        };
        for (index, sampler) in plan.samplers.iter().enumerate() {
            if let Some(cycle) = plan.tables[&sampler.table].cycle
                && sampler.row_cap().is_some()
                && plan.loops(index)
            {
                bail!(
                    "{}: a row limit can't be used on a sampler that depends on its own cycle ({}), \
                     because it runs in rounds",
                    sampler.origin,
                    plan.cycles[cycle].join(", ")
                );
            }
        }
        Ok(plan)
    }

    /// Returns the deps of sampler `index` whose parent is in the sampler's own cycle and doesn't
    /// keep every row. Those values grow while the cycle runs.
    ///
    /// # Panics
    ///
    /// Panics if `index` isn't a sampler of this plan.
    #[must_use]
    pub fn cycle_deps(&self, index: usize) -> Vec<&str> {
        let sampler = &self.samplers[index];
        let Some(cycle) = self.tables[&sampler.table].cycle else {
            return Vec::new();
        };
        sampler
            .of
            .iter()
            .filter(|(_, parent)| {
                self.tables[self.parent_table(parent)].cycle == Some(cycle)
                    && !self.allows_any(parent)
            })
            .map(|(column, _)| column.as_str())
            .collect()
    }

    /// Returns the table a dep reads from: the one it names, or the named sampler's table.
    #[must_use]
    pub fn parent_table<'a>(&'a self, parent: &'a ColumnRef) -> &'a str {
        match self.named.get(&parent.table) {
            Some(&index) => &self.samplers[index].table,
            None => &parent.table,
        }
    }

    /// Returns whether a dep may hold any non-NULL value, because the rows it reads from include
    /// every row of the parent.
    ///
    /// # Panics
    ///
    /// Panics if `parent` names neither a table nor a named sampler of this plan.
    #[must_use]
    pub fn allows_any(&self, parent: &ColumnRef) -> bool {
        match self.named.get(&parent.table) {
            Some(&index) => keeps_every_row(&self.samplers[index], self.since),
            None => self.tables[&parent.table].full,
        }
    }

    /// Returns the columns of `table` that references follow, from either side: the columns
    /// children follow, and the table's own `of` and `deps` columns. Sanitizing one would break a
    /// reference.
    #[must_use]
    pub fn references(&self, table: &str) -> BTreeSet<&str> {
        let mut columns: BTreeSet<&str> = BTreeSet::new();
        let Some(plan) = self.tables.get(table) else {
            return columns;
        };
        columns.extend(plan.keep.iter().map(String::as_str));
        for &index in &plan.samplers {
            let sampler = &self.samplers[index];
            columns.extend(sampler.of.keys().map(String::as_str));
            if let Some(name) = &sampler.name
                && let Some(kept) = self.named_keep.get(name)
            {
                columns.extend(kept.iter().map(String::as_str));
            }
        }
        // `deps` belong to the table, even on a sampler that takes no rows and so never runs.
        for sampler in self
            .samplers
            .iter()
            .filter(|sampler| sampler.table == table)
        {
            columns.extend(sampler.deps.keys().map(String::as_str));
        }
        // A dep on a table that takes every row pulls nothing in, so it isn't in `keep`.
        for sampler in &self.samplers {
            for parent in sampler.deps.values() {
                if parent.table == table {
                    columns.insert(&parent.column);
                }
            }
        }
        columns
    }

    /// Returns the `--since` condition on sampler `index`'s `limit.since`, if it has one.
    ///
    /// # Panics
    ///
    /// Panics if `index` isn't a sampler of this plan.
    #[must_use]
    pub fn time_filter(&self, index: usize, dialect: Dialect) -> Option<String> {
        self.since?
            .condition(dialect, self.samplers[index].since_columns())
    }

    /// Returns whether sampler `index` runs in rounds until its cycle stops growing.
    ///
    /// # Panics
    ///
    /// Panics if `index` isn't a sampler of this plan.
    #[must_use]
    pub fn loops(&self, index: usize) -> bool {
        !self.cycle_deps(index).is_empty()
    }

    /// Fails unless `--since` was given or no sampler has a `limit.since`. Samplers with a
    /// `since` only run once `--since` says how far back to go, so a forgotten flag can't quietly
    /// copy whole tables.
    pub fn check_since(&self) -> Result<()> {
        if self.since.is_some() {
            return Ok(());
        }
        let timed: Vec<&str> = self
            .samplers
            .iter()
            .filter(|sampler| !sampler.since_columns().is_empty())
            .map(|sampler| sampler.origin.as_str())
            .collect();
        if let Some(first) = timed.first() {
            bail!(
                "{} sampler(s) have a limit with since, starting with {first}. Pass --since, such as \
                 --since '2 weeks', or --since ever to keep every row",
                timed.len()
            );
        }
        Ok(())
    }

    /// The SELECT that sampler `index` runs, without its LIMIT, with each list of values written
    /// as the column they come from, as in `IN (<users.id>)`.
    #[must_use]
    pub fn query_outline(&self, index: usize, dialect: Dialect) -> Vec<u8> {
        let sampler = &self.samplers[index];
        let deps: Vec<Dep> = sampler
            .of
            .iter()
            .map(|(column, parent)| {
                let allowed = if self.allows_any(parent) {
                    Allowed::AnyValue
                } else {
                    Allowed::Values(vec![format!("<{parent}>").into_bytes()])
                };
                Dep {
                    column: column.clone(),
                    allowed,
                }
            })
            .collect();
        // Every dep allows one placeholder value, so there's exactly one query.
        let time_filter = self.time_filter(index, dialect);
        sql::sampler_queries(dialect, sampler, time_filter.as_deref(), &deps, 1).remove(0)
    }

    /// Returns what `--plan` prints: each table in run order, with the queries its samplers will
    /// run.
    #[must_use]
    pub fn describe(&self, dialect: Dialect) -> String {
        let timed = self
            .samplers
            .iter()
            .filter(|sampler| !sampler.since_columns().is_empty())
            .count();
        let limited = self
            .samplers
            .iter()
            .filter(|sampler| {
                self.since
                    .is_some_and(|since| since.narrows(sampler.since_columns()))
            })
            .count();
        let mut out = format!(
            "{} tables, {} samplers, {timed} with a since\n",
            self.tables.len(),
            self.samplers.len()
        );
        let since_line: String = match self.since {
            None if timed > 0 => "--since: not given, so a run would refuse to start\n".to_string(),
            None => "--since: not needed\n".to_string(),
            Some(Since::Ever) => "--since ever: limit since ignored, every row kept\n".to_string(),
            Some(since @ Since::Back { .. }) => {
                format!("--since {since}: limits {limited} samplers\n")
            }
        };
        out.push_str(&since_line);
        if let Some(rule) = self.sanitize.columns.first() {
            out.push_str(&format!(
                "sanitize: {} rules for every table, from {}\n",
                self.sanitize.columns.len(),
                rule.file
            ));
        }

        for name in &self.order {
            let table = &self.tables[name];
            let mut notes: Vec<String> = Vec::new();
            if let Some(cycle) = table.cycle {
                notes.push(format!("cycle: {}", self.cycles[cycle].join(", ")));
            }
            if !table.parents.is_empty() {
                notes.push(format!("after {}", join(&table.parents)));
            }
            if !table.keep.is_empty() {
                notes.push(format!("keeps {}", join(&table.keep)));
            }
            if table.full {
                notes.push("every row".to_string());
            }
            if table.samplers.is_empty() {
                notes.push("no rows".to_string());
            }
            if let Some(rules) = self.sanitize.tables.get(name) {
                let rules: Vec<String> = rules
                    .iter()
                    .map(|rule| format!("{}: {}", rule.pattern, rule.replacement))
                    .collect();
                notes.push(format!("sanitizes {}", rules.join(", ")));
            }
            out.push_str(&format!("\n{name}"));
            if !notes.is_empty() {
                out.push_str(&format!("  ({})", notes.join("; ")));
            }
            out.push('\n');

            for &index in &table.samplers {
                let sampler = &self.samplers[index];
                let mut query = self.query_outline(index, dialect);
                if let Some(limit) = sampler.row_cap() {
                    query.extend_from_slice(format!(" LIMIT {limit}").as_bytes());
                }
                let rounds = if self.loops(index) {
                    "  (in rounds)"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "  {}{rounds}\n    {}\n",
                    sampler.origin,
                    String::from_utf8_lossy(&query)
                ));
            }
        }
        out
    }
}

/// Turns `deps` into samplers. A dep says this table's rows point at the parent's rows, so the
/// parent gets a sampler for exactly the rows they point at: `of: { parent_column: table.column }`.
/// Deps belong to the table, so the pulled rows include those pointed at by rows other samplers
/// (pulls included) brought in. A parent that keeps every row needs no pull.
fn pull_referenced_rows(samplers: &mut Vec<Sampler>, since: Option<Since>) -> Result<()> {
    let names: BTreeSet<&str> = samplers
        .iter()
        .filter_map(|sampler| sampler.name.as_deref())
        .collect();
    let full: BTreeSet<&str> = samplers
        .iter()
        .filter(|sampler| keeps_every_row(sampler, since))
        .map(|sampler| sampler.table.as_str())
        .collect();

    // (child table, child column) -> (parent table, parent column), each once.
    let mut edges: BTreeMap<(String, String), (ColumnRef, &str)> = BTreeMap::new();
    for sampler in samplers.iter() {
        for (column, parent) in &sampler.deps {
            if names.contains(parent.table.as_str()) {
                bail!(
                    "{}: deps point at tables, but `{}` is a sampler name. Use `of` to follow a \
                     named sampler's rows",
                    sampler.origin,
                    parent.table
                );
            }
            let key = (sampler.table.clone(), column.clone());
            if let Some((other, origin)) = edges.get(&key)
                && other != parent
            {
                bail!(
                    "{}: {}.{column} points at {parent}, but {origin} says it points at {other}",
                    sampler.origin,
                    sampler.table
                );
            }
            edges.insert(key, (parent.clone(), sampler.origin.as_str()));
        }
    }

    let pulls: Vec<Sampler> = edges
        .into_iter()
        .filter(|(_, (parent, _))| !full.contains(parent.table.as_str()))
        .map(|((table, column), (parent, _))| Sampler {
            origin: format!("pulled in by {table}.{column}"),
            table: parent.table,
            of: BTreeMap::from([(parent.column, ColumnRef { table, column })]),
            ..Sampler::default()
        })
        .collect();
    samplers.extend(pulls);
    Ok(())
}

/// Returns whether `sampler` keeps every row of its table once `since` applies.
#[must_use]
fn keeps_every_row(sampler: &Sampler, since: Option<Since>) -> bool {
    sampler.is_unfiltered() && !since.is_some_and(|since| since.narrows(sampler.since_columns()))
}

#[must_use]
fn join(names: &BTreeSet<String>) -> String {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.join(", ")
}

/// Groups tables into strongly connected components with Tarjan's algorithm. Returns the tables
/// in an order where every table comes after the tables it depends on (members of a cycle sit
/// together, by name), and the cycles: components of several tables, or of one that depends on
/// itself.
#[must_use]
fn components(tables: &BTreeMap<String, TablePlan>) -> (Vec<String>, Vec<Vec<String>>) {
    struct Tarjan<'a> {
        tables: &'a BTreeMap<String, TablePlan>,
        next: usize,
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: BTreeSet<&'a str>,
        found: Vec<Vec<&'a str>>,
    }

    impl<'a> Tarjan<'a> {
        fn visit(&mut self, name: &'a str) {
            self.index.insert(name, self.next);
            self.low.insert(name, self.next);
            self.next += 1;
            self.stack.push(name);
            self.on_stack.insert(name);

            for child in &self.tables[name].children {
                let child = child.as_str();
                if !self.index.contains_key(child) {
                    self.visit(child);
                    let low = self.low[name].min(self.low[child]);
                    self.low.insert(name, low);
                } else if self.on_stack.contains(child) {
                    let low = self.low[name].min(self.index[child]);
                    self.low.insert(name, low);
                }
            }

            if self.low[name] == self.index[name] {
                let mut component = Vec::new();
                loop {
                    let member = self
                        .stack
                        .pop()
                        .expect("`name` is on the stack until its component is popped");
                    self.on_stack.remove(member);
                    component.push(member);
                    if member == name {
                        break;
                    }
                }
                component.sort();
                self.found.push(component);
            }
        }
    }

    let mut tarjan = Tarjan {
        tables,
        next: 0,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        found: Vec::new(),
    };
    for name in tables.keys() {
        if !tarjan.index.contains_key(name.as_str()) {
            tarjan.visit(name);
        }
    }

    // Tarjan finds a component only after everything it leads to, so reversed, parents come first.
    let mut order = Vec::with_capacity(tables.len());
    let mut cycles = Vec::new();
    for component in tarjan.found.into_iter().rev() {
        let names: Vec<String> = component.iter().map(|name| name.to_string()).collect();
        let cyclic = names.len() > 1 || tables[&names[0]].parents.contains(&names[0]);
        if cyclic {
            cycles.push(names.clone());
        }
        order.extend(names);
    }
    (order, cycles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(yaml: &str) -> Result<Plan> {
        let config: Config = yaml.parse()?;
        Plan::try_new(config, None)
    }

    fn plan_since(yaml: &str, since: &str) -> Result<Plan> {
        let config: Config = yaml.parse()?;
        let since: Since = since.parse().map_err(anyhow::Error::msg)?;
        Plan::try_new(config, Some(since))
    }

    /// Returns where `table` sits in the plan's run order.
    fn position(plan: &Plan, table: &str) -> Result<usize> {
        use anyhow::Context;

        plan.order
            .iter()
            .position(|name| name == table)
            .with_context(|| format!("{table} isn't in the run order"))
    }

    const TIMED: &str = r#"
samplers:
  - table: countries
    limit: all
  - table: users
    limit: { since: [created_at, updated_at] }
    of:
      country_id: countries.id
  - table: orders
    limit: { since: created_at }
    of:
      user_id: users.id
"#;

    #[test]
    fn applies_since_to_samplers_with_a_since_limit() -> Result<()> {
        let plan = plan_since(TIMED, "2 weeks")?;
        assert_eq!(plan.time_filter(0, Dialect::MySql), None);
        assert_eq!(
            plan.time_filter(1, Dialect::MySql).as_deref(),
            Some(
                "(`created_at` >= CURDATE() - INTERVAL 2 WEEK OR `updated_at` >= CURDATE() - INTERVAL 2 WEEK)"
            )
        );
        assert_eq!(
            plan.time_filter(2, Dialect::MySql).as_deref(),
            Some("`created_at` >= CURDATE() - INTERVAL 2 WEEK")
        );
        plan.check_since()
    }

    #[test]
    fn leaves_the_configured_samplers_as_they_were() -> Result<()> {
        let config: Config = TIMED.parse()?;
        let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
        let plan = Plan::try_new(config.clone(), Some(since))?;
        assert_eq!(
            plan.samplers, config.samplers,
            "--since lives in the plan, not in the samplers"
        );
        Ok(())
    }

    #[test]
    fn a_time_limited_table_is_not_full() -> Result<()> {
        let yaml = "samplers:\n  - table: events\n    limit: { since: created_at }\n";
        assert!(
            !plan_since(yaml, "1 week")?.tables["events"].full,
            "--since 1 week drops older events"
        );
        assert!(
            plan_since(yaml, "ever")?.tables["events"].full,
            "--since ever keeps every event"
        );
        Ok(())
    }

    #[test]
    fn since_ever_keeps_every_row() -> Result<()> {
        let plan = plan_since(TIMED, "ever")?;
        let filters: Vec<Option<String>> = (0..plan.samplers.len())
            .map(|index| plan.time_filter(index, Dialect::MySql))
            .collect();
        assert!(
            filters.iter().all(|filter| filter.is_none()),
            "--since ever needs no time filter: {filters:?}"
        );
        plan.check_since()
    }

    #[test]
    fn requires_since_when_a_sampler_has_a_since_limit() -> Result<()> {
        let error = plan(TIMED)?
            .check_since()
            .expect_err("TIMED has time columns and no --since")
            .to_string();
        assert!(
            error.contains("2 sampler(s) have a limit with since"),
            "{error}"
        );
        assert!(error.contains("config sampler #2 (users)"), "{error}");
        assert!(error.contains("--since ever"), "{error}");

        plan(SHOP)?.check_since()
    }

    #[test]
    fn describes_the_plan() -> Result<()> {
        let described = plan_since(TIMED, "2 weeks")?.describe(Dialect::MySql);
        assert!(
            described.contains("3 tables, 3 samplers, 2 with a since"),
            "{described}"
        );
        assert!(
            described.contains("--since 2 weeks: limits 2 samplers"),
            "{described}"
        );
        assert!(
            described.contains("users  (after countries; keeps id)"),
            "{described}"
        );
        assert!(
            described.contains(
                "SELECT * FROM `orders` WHERE `created_at` >= CURDATE() - INTERVAL 2 WEEK \
                 AND `user_id` IN (<users.id>)"
            ),
            "{described}"
        );
        assert!(
            described.contains("`country_id` IS NOT NULL"),
            "{described}"
        );

        let unset = plan(TIMED)?.describe(Dialect::MySql);
        assert!(unset.contains("a run would refuse to start"), "{unset}");
        Ok(())
    }

    #[test]
    fn describes_queries_as_the_source_runs_them() -> Result<()> {
        let described = plan_since(TIMED, "2 weeks")?.describe(Dialect::Postgres);
        assert!(
            described.contains(
                "SELECT * FROM \"orders\" WHERE \"created_at\" >= CURRENT_DATE - INTERVAL '2 WEEK' \
                 AND \"user_id\" IN (<users.id>)"
            ),
            "{described}"
        );
        Ok(())
    }

    const SHOP: &str = r#"
samplers:
  - table: countries
    limit: all
  - table: users
    order_by: id DESC
    limit: 100
    of:
      country_id: countries.id
  - table: users
    limit: all
    where: admin = 1
  - table: phones
    of:
      user_id: users.id
  - table: orders
    of:
      user_id: users.id
  - table: line_items
    of:
      order_id: orders.id
"#;

    #[test]
    fn orders_parents_before_children() -> Result<()> {
        let plan = plan(SHOP)?;

        assert_eq!(plan.order.len(), 5);
        for (parent, child) in [
            ("countries", "users"),
            ("users", "phones"),
            ("users", "orders"),
            ("orders", "line_items"),
        ] {
            assert!(
                position(&plan, parent)? < position(&plan, child)?,
                "{parent} must run before {child}: {:?}",
                plan.order
            );
        }
        Ok(())
    }

    #[test]
    fn tracks_kept_columns_full_tables_and_dedupe() -> Result<()> {
        let plan = plan(SHOP)?;

        assert_eq!(
            plan.tables["users"].keep,
            BTreeSet::from(["id".to_string()])
        );
        assert_eq!(
            plan.tables["orders"].keep,
            BTreeSet::from(["id".to_string()])
        );
        assert!(
            plan.tables["phones"].keep.is_empty(),
            "nothing depends on phones, yet it keeps {:?}",
            plan.tables["phones"].keep
        );

        assert!(
            plan.tables["countries"].full,
            "countries' only sampler keeps every row"
        );
        assert!(
            !plan.tables["users"].full,
            "neither users sampler keeps every row"
        );

        assert!(
            plan.tables["users"].dedupe,
            "users has two samplers that can return the same row"
        );
        assert!(
            !plan.tables["orders"].dedupe,
            "orders has one sampler and no cycle"
        );
        Ok(())
    }

    #[test]
    fn knows_which_columns_references_follow() -> Result<()> {
        let plan = plan(
            r#"
samplers:
  - table: written_off_projects
    limit: all
    name: recent_write_offs
    where: created_at > '2024-01-01'
  - table: projects
    of:
      id: recent_write_offs.project_id
  - table: orders
    limit: all
    deps:
      user_id: users.id
  - table: countries
    limit: all
  - table: addresses
    limit: all
    deps:
      country_code: countries.code
"#,
        )?;
        let references = |table: &str| -> Vec<String> {
            plan.references(table)
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        // Followed by a child through a named sampler, by the child's own `of`, by `deps` from
        // the child's side, and on the parent's side.
        assert_eq!(references("written_off_projects"), ["project_id"]);
        assert_eq!(references("projects"), ["id"]);
        assert_eq!(references("orders"), ["user_id"]);
        assert_eq!(references("users"), ["id"]);
        // countries takes every row, so the dep pulls nothing in, but it's still a reference.
        assert_eq!(references("addresses"), ["country_code"]);
        assert_eq!(references("countries"), ["code"]);
        Ok(())
    }

    #[test]
    fn rejects_sanitize_rules_for_tables_nothing_extracts() {
        let error = plan(
            "samplers:\n  - table: users\n    limit: all\nsanitize:\n  tables:\n    user:\n      email: null\n",
        )
        .expect_err("no sampler extracts `user`");
        assert_eq!(
            error.to_string(),
            "config: sanitize.tables.user names a table no sampler extracts"
        );
    }

    #[test]
    fn describes_sanitize_rules() -> Result<()> {
        let described = plan(
            r#"
samplers:
  - table: users
    limit: all
  - table: orders
    limit: all
sanitize:
  columns:
    "*email*": { fake: email }
    ssn: null
  tables:
    users:
      login: { template: "user_{id}" }
      retain_email: keep
"#,
        )?
        .describe(Dialect::MySql);
        assert!(
            described.contains("sanitize: 2 rules for every table, from config\n"),
            "{described}"
        );
        assert!(
            described.contains(
                "users  (every row; sanitizes login: template user_{id}, retain_email: keep)"
            ),
            "{described}"
        );
        assert!(
            described.contains("orders  (every row)\n"),
            "orders has no rules of its own: {described}"
        );

        let plain = plan(SHOP)?.describe(Dialect::MySql);
        assert!(!plain.contains("sanitize"), "{plain}");
        Ok(())
    }

    #[test]
    fn rejects_missing_parents() {
        let error = plan("samplers:\n  - table: phones\n    of:\n      user_id: users.id\n")
            .expect_err("no sampler extracts users");
        assert!(
            error.to_string().contains("no sampler extracts users"),
            "{error}"
        );
    }

    const PROJECTS: &str = r#"
samplers:
  - table: users
    limit: all
  - table: project_tasks
    limit: { since: updated_at }
  - table: projects
    of:
      id: project_tasks.project_id
  - table: project_tasks
    of:
      project_id: projects.id
  - table: task_notes
    of:
      task_id: project_tasks.id
  - table: projects
    of:
      owner_id: users.id
      id: project_tasks.project_id
"#;

    #[test]
    fn groups_tables_that_depend_on_each_other() -> Result<()> {
        let plan = plan_since(PROJECTS, "2 weeks")?;

        assert_eq!(plan.cycles, [["project_tasks", "projects"]]);
        assert_eq!(plan.tables["projects"].cycle, Some(0));
        assert_eq!(plan.tables["project_tasks"].cycle, Some(0));
        assert_eq!(plan.tables["users"].cycle, None);
        assert!(
            plan.tables["project_tasks"].dedupe,
            "a table in a cycle can return the same row in several rounds"
        );

        for (parent, child) in [
            ("users", "projects"),
            ("project_tasks", "task_notes"),
            ("projects", "task_notes"),
        ] {
            assert!(
                position(&plan, parent)? < position(&plan, child)?,
                "{parent} must run before {child}: {:?}",
                plan.order
            );
        }
        Ok(())
    }

    #[test]
    fn knows_which_samplers_run_in_rounds() -> Result<()> {
        let plan = plan_since(PROJECTS, "2 weeks")?;
        assert!(!plan.loops(1), "the timed project_tasks sampler is a seed");
        assert!(
            plan.loops(2),
            "projects depends on project_tasks in its cycle"
        );
        assert!(
            plan.loops(3),
            "project_tasks depends on projects in its cycle"
        );
        assert!(!plan.loops(4), "task_notes isn't in the cycle");
        assert_eq!(plan.cycle_deps(5), ["id"], "users.id is outside the cycle");
        Ok(())
    }

    #[test]
    fn a_cycle_needs_no_rounds_when_since_keeps_everything() -> Result<()> {
        // The timed project_tasks sampler keeps every row, so deps on project_tasks need no values.
        let plan = plan_since(PROJECTS, "ever")?;
        assert!(
            plan.tables["project_tasks"].full,
            "--since ever makes the timed project_tasks sampler keep every row"
        );
        assert!(!plan.loops(2), "projects' only cycle dep allows any value");
        assert!(plan.loops(3), "projects still isn't full");
        Ok(())
    }

    #[test]
    fn plans_self_references() -> Result<()> {
        let plan = plan(
            r#"
samplers:
  - table: employees
    limit: 10
  - table: employees
    of:
      id: employees.manager_id
"#,
        )?;
        assert_eq!(plan.cycles, [["employees"]]);
        assert!(!plan.loops(0), "the first sampler has no deps");
        assert!(
            plan.loops(1),
            "managers of extracted employees are extracted in rounds"
        );
        Ok(())
    }

    #[test]
    fn a_dep_on_a_full_table_in_the_cycle_does_not_loop() -> Result<()> {
        let plan = plan(
            r#"
samplers:
  - table: categories
    limit: all
  - table: categories
    of:
      parent_id: categories.id
"#,
        )?;
        assert_eq!(plan.cycles, [["categories"]]);
        assert!(
            !plan.loops(1),
            "every category is extracted, so any parent_id is fine"
        );
        Ok(())
    }

    #[test]
    fn rejects_limits_on_samplers_that_loop() {
        let error = plan(
            r#"
samplers:
  - table: employees
    limit: 10
  - table: employees
    limit: 5
    of:
      id: employees.manager_id
"#,
        )
        .expect_err("the second sampler loops and has a limit");
        assert!(
            error.to_string().contains("a row limit can't be used"),
            "{error}"
        );
        assert!(error.to_string().contains("sampler #2"), "{error}");
    }

    #[test]
    fn describes_cycles() -> Result<()> {
        let described = plan_since(PROJECTS, "2 weeks")?.describe(Dialect::MySql);
        assert!(
            described.contains("projects  (cycle: project_tasks, projects;"),
            "{described}"
        );
        assert!(
            described.contains("sampler #3 (projects)  (in rounds)"),
            "{described}"
        );
        Ok(())
    }

    const WRITE_OFFS: &str = r#"
samplers:
  - table: written_off_projects
    limit: all
  - table: written_off_projects
    name: recent_write_offs
    limit: { since: updated_at }
  - table: projects
    of:
      id: recent_write_offs.project_id
"#;

    #[test]
    fn deps_can_name_a_sampler() -> Result<()> {
        let plan = plan_since(WRITE_OFFS, "2 weeks")?;
        assert_eq!(plan.named["recent_write_offs"], 1);
        assert_eq!(
            plan.named_keep["recent_write_offs"],
            BTreeSet::from(["project_id".to_string()])
        );
        assert!(
            plan.tables["written_off_projects"].keep.is_empty(),
            "the named sampler keeps the values, not the table: {:?}",
            plan.tables["written_off_projects"].keep
        );
        assert!(
            plan.tables["written_off_projects"].full,
            "the unnamed sampler keeps every write-off"
        );
        assert_eq!(
            plan.tables["projects"].parents,
            BTreeSet::from(["written_off_projects".to_string()])
        );

        // The table keeps every row, but the named sampler doesn't, so its values matter.
        let parent = &plan.samplers[2].of["id"];
        assert_eq!(plan.parent_table(parent), "written_off_projects");
        assert!(
            !plan.allows_any(parent),
            "recent_write_offs only keeps recent rows"
        );
        let described = plan.describe(Dialect::MySql);
        assert!(
            described.contains("`id` IN (<recent_write_offs.project_id>)"),
            "{described}"
        );
        Ok(())
    }

    #[test]
    fn a_named_sampler_that_keeps_everything_allows_any_value() -> Result<()> {
        let plan = plan_since(WRITE_OFFS, "ever")?;
        assert!(
            plan.allows_any(&plan.samplers[2].of["id"]),
            "--since ever makes recent_write_offs keep every row"
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_names() {
        let clash = "samplers:\n  - table: users\n    limit: all\n  - table: orders\n    limit: all\n    name: users\n";
        let error = plan(clash)
            .expect_err("a sampler can't be named after a table")
            .to_string();
        assert!(error.contains("name `users` is also a table"), "{error}");

        let twice = "samplers:\n  - table: users\n    limit: all\n    name: a\n  - table: orders\n    limit: all\n    name: a\n";
        let error = plan(twice)
            .expect_err("two samplers can't share a name")
            .to_string();
        assert!(error.contains("name `a` is already used"), "{error}");
    }

    /// The example from the design discussion, as written.
    const REFERENCES: &str = r#"
samplers:
  - table: users
    limit: all
    deps:
      territory_id: territories.id
    where: age > 25

  - table: orders
    deps:
      user_id: users.id
    limit: { since: created_at }

  - table: territories
    limit: all
"#;

    #[test]
    fn deps_pull_in_the_rows_they_point_at() -> Result<()> {
        let plan = plan_since(REFERENCES, "2 weeks")?;

        // users: its own sampler, plus one for the users the orders point at.
        let users: Vec<&Sampler> = plan.tables["users"]
            .samplers
            .iter()
            .map(|&index| &plan.samplers[index])
            .collect();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].filter.as_deref(), Some("age > 25"));
        assert!(
            users[0].of.is_empty(),
            "deps don't narrow the users sampler"
        );
        assert_eq!(users[1].origin, "pulled in by orders.user_id");
        assert_eq!(users[1].of["id"].to_string(), "orders.user_id");

        // orders: only its time window; it runs before the users it points at.
        let orders_index = plan.tables["orders"].samplers[0];
        let orders = &plan.samplers[orders_index];
        assert!(
            orders.of.is_empty(),
            "deps don't narrow the orders sampler: {:?}",
            orders.of
        );
        assert!(
            plan.time_filter(orders_index, Dialect::MySql).is_some(),
            "orders has a since and --since is given"
        );
        assert_eq!(
            plan.tables["orders"].keep,
            BTreeSet::from(["user_id".to_string()])
        );
        assert!(
            position(&plan, "orders")? < position(&plan, "users")?,
            "orders must run before the users they pull in: {:?}",
            plan.order
        );

        // territories keeps every row, so nothing needs pulling there.
        assert_eq!(plan.tables["territories"].samplers.len(), 1);
        assert!(
            plan.cycles.is_empty(),
            "nothing depends on itself: {:?}",
            plan.cycles
        );
        Ok(())
    }

    #[test]
    fn a_referenced_table_needs_no_sampler_of_its_own() -> Result<()> {
        let plan = plan_since(
            "samplers:\n  - table: orders\n    limit: { since: created_at }\n    deps:\n      user_id: users.id\n",
            "1 week",
        )?;
        assert_eq!(plan.tables["users"].samplers.len(), 1);
        assert!(
            !plan.tables["users"].full,
            "the pulled users sampler only keeps users that orders point at"
        );
        Ok(())
    }

    const INVOICES: &str = r#"
samplers:
  - table: invoices
    deps:
      customer_id: customers.id
  - table: orders
    limit: all
    deps:
      invoice_id: invoices.id
"#;

    #[test]
    fn a_sampler_with_only_deps_takes_no_rows_but_its_deps_still_pull() -> Result<()> {
        let plan = plan(INVOICES)?;

        // Orders pull the invoices they point at; the declared invoices sampler never runs.
        assert_eq!(
            plan.tables["invoices"].samplers.len(),
            1,
            "only the sampler pulled in by orders.invoice_id runs"
        );
        let pulled = &plan.samplers[plan.tables["invoices"].samplers[0]];
        assert_eq!(pulled.origin, "pulled in by orders.invoice_id");
        assert!(
            !plan.tables["invoices"].full,
            "invoices keeps no rows of its own"
        );
        // Those invoices bring their customers.
        assert_eq!(plan.tables["customers"].samplers.len(), 1);
        assert_eq!(
            plan.samplers[plan.tables["customers"].samplers[0]].origin,
            "pulled in by invoices.customer_id"
        );
        Ok(())
    }

    #[test]
    fn a_table_with_only_a_deps_sampler_has_no_rows() -> Result<()> {
        let plan =
            plan("samplers:\n  - table: invoices\n    deps:\n      customer_id: customers.id\n")?;

        assert!(
            plan.tables["invoices"].samplers.is_empty(),
            "nothing runs for invoices: {:?}",
            plan.tables["invoices"]
        );
        assert!(!plan.tables["invoices"].full);
        let described = plan.describe(Dialect::MySql);
        assert!(
            described.contains("invoices  (keeps customer_id; no rows)"),
            "{described}"
        );
        Ok(())
    }

    #[test]
    fn deps_of_a_sampler_that_takes_no_rows_are_references() -> Result<()> {
        let plan =
            plan("samplers:\n  - table: invoices\n    deps:\n      customer_id: customers.id\n")?;

        assert_eq!(
            plan.references("invoices"),
            BTreeSet::from(["customer_id"]),
            "sanitizing customer_id would break the reference"
        );
        Ok(())
    }

    #[test]
    fn a_row_cap_and_since_combine_into_one_query() -> Result<()> {
        let plan = plan_since(
            "samplers:\n  - table: events\n    order_by: id DESC\n    limit:\n      rows: 100\n      since: created_at\n",
            "2 weeks",
        )?;

        let described = plan.describe(Dialect::MySql);
        assert!(
            described.contains(
                "SELECT * FROM `events` WHERE `created_at` >= CURDATE() - INTERVAL 2 WEEK \
                 ORDER BY id DESC LIMIT 100"
            ),
            "{described}"
        );
        assert!(!plan.tables["events"].full);
        Ok(())
    }

    #[test]
    fn since_limits_are_fine_in_a_cycle_but_row_caps_are_not() -> Result<()> {
        let cycle = |limit: &str| {
            plan(&format!(
                "samplers:\n  - table: employees\n    where: id = 40\n    limit: all\n  - table: employees\n    limit: {limit}\n    of:\n      id: employees.manager_id\n"
            ))
        };

        let planned = cycle("{ since: updated_at }")?;
        assert!(planned.loops(1), "the second sampler runs in rounds");
        let error = cycle("{ rows: 5, since: updated_at }")
            .expect_err("a row cap can't hold across rounds");
        assert!(
            error.to_string().contains("a row limit can't be used"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn deps_on_a_table_that_points_at_itself_form_a_cycle() -> Result<()> {
        let plan = plan(
            "samplers:\n  - table: employees\n    limit: all\n    where: id = 40\n    deps:\n      manager_id: employees.id\n",
        )?;
        assert_eq!(plan.cycles, [["employees"]]);
        assert!(
            plan.loops(1),
            "the pull follows managers of pulled managers"
        );
        Ok(())
    }

    #[test]
    fn rejects_deps_on_sampler_names_and_conflicting_deps() {
        let named = "samplers:\n  - table: users\n    limit: all\n    name: adults\n  - table: orders\n    limit: all\n    deps:\n      user_id: adults.id\n";
        let error = plan(named)
            .expect_err("deps can't point at a sampler name")
            .to_string();
        assert!(error.contains("Use `of`"), "{error}");

        let conflict = "samplers:\n  - table: orders\n    limit: all\n    deps:\n      user_id: users.id\n  - table: orders\n    limit: all\n    where: total > 0\n    deps:\n      user_id: people.id\n";
        let error = plan(conflict)
            .expect_err("orders.user_id can't point at two tables")
            .to_string();
        assert!(
            error.contains("orders.user_id points at people.id"),
            "{error}"
        );
    }
}
