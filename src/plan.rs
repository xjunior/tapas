use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::config::{ColumnRef, Config, Sampler};
use crate::since::Since;
use crate::sql::{self, Allowed};

/// What the extraction needs to know about one table.
#[derive(Debug, Default)]
pub struct TablePlan {
    /// Indexes into `Plan::samplers`.
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

#[derive(Debug)]
pub struct Plan {
    pub samplers: Vec<Sampler>,
    pub tables: BTreeMap<String, TablePlan>,
    /// Tables in an order where each comes after its parents; tables in a cycle sit together.
    pub order: Vec<String>,
    /// Groups of tables that depend on each other. They're extracted together, in rounds.
    pub cycles: Vec<Vec<String>>,
    /// Sampler name -> index into `samplers`.
    pub named: BTreeMap<String, usize>,
    /// Columns whose values children read from a named sampler's own rows.
    pub named_keep: BTreeMap<String, BTreeSet<String>>,
    pub since: Option<Since>,
}

impl Plan {
    pub fn build(config: Config, since: Option<Since>) -> Result<Self> {
        let mut samplers = config.samplers;
        if let Some(since) = since {
            for sampler in samplers.iter_mut() {
                sampler.time_filter = since.condition(&sampler.time_column);
            }
        }

        pull_referenced_rows(&mut samplers)?;

        let mut tables: BTreeMap<String, TablePlan> = BTreeMap::new();

        for (index, sampler) in samplers.iter().enumerate() {
            let table = tables.entry(sampler.table.clone()).or_default();
            table.samplers.push(index);
            table.full |= sampler.is_full();
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
                    .unwrap()
                    .children
                    .insert(sampler.table.clone());
                tables
                    .get_mut(&sampler.table)
                    .unwrap()
                    .parents
                    .insert(parent_table);
            }
        }

        let (order, cycles) = components(&tables);
        for (index, cycle) in cycles.iter().enumerate() {
            for name in cycle {
                tables.get_mut(name).unwrap().cycle = Some(index);
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
        };
        for (index, sampler) in plan.samplers.iter().enumerate() {
            if plan.loops(index) && sampler.limit.is_some() {
                bail!(
                    "{}: limit can't be used on a sampler that depends on its own cycle ({}), \
                     because it runs in rounds",
                    sampler.origin,
                    plan.cycles[plan.tables[&sampler.table].cycle.unwrap()].join(", ")
                );
            }
        }
        Ok(plan)
    }

    /// The deps of sampler `index` whose parent is in the sampler's own cycle and doesn't keep
    /// every row. Those values grow while the cycle runs.
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

    /// The table a dep reads from: the one it names, or the named sampler's table.
    pub fn parent_table<'a>(&'a self, parent: &'a ColumnRef) -> &'a str {
        match self.named.get(&parent.table) {
            Some(&index) => &self.samplers[index].table,
            None => &parent.table,
        }
    }

    /// A dep on rows that include every row of the parent allows any non-NULL value.
    pub fn allows_any(&self, parent: &ColumnRef) -> bool {
        match self.named.get(&parent.table) {
            Some(&index) => self.samplers[index].is_full(),
            None => self.tables[&parent.table].full,
        }
    }

    /// Sampler `index` runs in rounds until its cycle stops growing.
    pub fn loops(&self, index: usize) -> bool {
        !self.cycle_deps(index).is_empty()
    }

    /// Samplers with a `time_column` only run once `--since` says how far back to go, so a
    /// forgotten flag can't quietly copy whole tables.
    pub fn check_since(&self) -> Result<()> {
        if self.since.is_some() {
            return Ok(());
        }
        let timed: Vec<&str> = self
            .samplers
            .iter()
            .filter(|sampler| !sampler.time_column.is_empty())
            .map(|sampler| sampler.origin.as_str())
            .collect();
        if let Some(first) = timed.first() {
            bail!(
                "{} sampler(s) have a time_column, starting with {first}. Pass --since, such as \
                 --since '2 weeks', or --since ever to keep every row",
                timed.len()
            );
        }
        Ok(())
    }

    /// What `--plan` prints: each table in run order, with the queries its samplers will run.
    pub fn describe(&self) -> String {
        let timed = self
            .samplers
            .iter()
            .filter(|s| !s.time_column.is_empty())
            .count();
        let limited = self
            .samplers
            .iter()
            .filter(|s| s.time_filter.is_some())
            .count();
        let mut out = format!(
            "{} tables, {} samplers, {timed} with a time_column\n",
            self.tables.len(),
            self.samplers.len()
        );
        out.push_str(&match self.since {
            None if timed > 0 => "--since: not given, so a run would refuse to start\n".into(),
            None => "--since: not needed\n".into(),
            Some(Since::Ever) => "--since ever: time_column ignored, every row kept\n".into(),
            Some(since) => format!("--since {since}: limits {limited} samplers\n"),
        });

        for name in &self.order {
            let table = &self.tables[name];
            let mut notes = Vec::new();
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
                notes.push("every row".into());
            }
            out.push_str(&format!("\n{name}"));
            if !notes.is_empty() {
                out.push_str(&format!("  ({})", notes.join("; ")));
            }
            out.push('\n');

            for &index in &table.samplers {
                let sampler = &self.samplers[index];
                let deps: Vec<(String, Allowed)> = sampler
                    .of
                    .iter()
                    .map(|(column, parent)| {
                        let allowed = if self.allows_any(parent) {
                            Allowed::AnyValue
                        } else {
                            Allowed::Values(vec![format!("<{parent}>").into_bytes()])
                        };
                        (column.clone(), allowed)
                    })
                    .collect();
                let mut query = sql::sampler_queries(sampler, &deps, 1).remove(0);
                if let Some(limit) = sampler.limit {
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
fn pull_referenced_rows(samplers: &mut Vec<Sampler>) -> Result<()> {
    let names: BTreeSet<&str> = samplers.iter().filter_map(|s| s.name.as_deref()).collect();
    let full: BTreeSet<&str> = samplers
        .iter()
        .filter(|s| s.is_full())
        .map(|s| s.table.as_str())
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
            name: None,
            filter: None,
            order_by: None,
            limit: None,
            of: BTreeMap::from([(parent.column, ColumnRef { table, column })]),
            deps: BTreeMap::new(),
            time_column: Vec::new(),
            time_filter: None,
        })
        .collect();
    samplers.extend(pulls);
    Ok(())
}

fn join(names: &BTreeSet<String>) -> String {
    names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Groups tables into strongly connected components with Tarjan's algorithm. Returns the tables
/// in an order where every table comes after the tables it depends on (members of a cycle sit
/// together, by name), and the cycles: components of several tables, or of one that depends on
/// itself.
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
                    let member = self.stack.pop().unwrap();
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
        Plan::build(Config::parse(yaml)?, None)
    }

    fn plan_since(yaml: &str, since: &str) -> Plan {
        Plan::build(Config::parse(yaml).unwrap(), Some(since.parse().unwrap())).unwrap()
    }

    const TIMED: &str = r#"
samplers:
  - table: countries
  - table: users
    time_column: [created_at, updated_at]
    of:
      country_id: countries.id
  - table: orders
    time_column: created_at
    of:
      user_id: users.id
"#;

    #[test]
    fn applies_since_to_samplers_with_a_time_column() {
        let plan = plan_since(TIMED, "2 weeks");
        assert_eq!(plan.samplers[0].time_filter, None);
        assert_eq!(
            plan.samplers[1].time_filter.as_deref(),
            Some(
                "(`created_at` >= CURDATE() - INTERVAL 2 WEEK OR `updated_at` >= CURDATE() - INTERVAL 2 WEEK)"
            )
        );
        assert_eq!(
            plan.samplers[2].time_filter.as_deref(),
            Some("`created_at` >= CURDATE() - INTERVAL 2 WEEK")
        );
        assert!(plan.check_since().is_ok());
    }

    #[test]
    fn a_time_limited_table_is_not_full() {
        let yaml = "samplers:\n  - table: events\n    time_column: created_at\n";
        assert!(!plan_since(yaml, "1 week").tables["events"].full);
        assert!(plan_since(yaml, "ever").tables["events"].full);
    }

    #[test]
    fn since_ever_keeps_every_row() {
        let plan = plan_since(TIMED, "ever");
        assert!(
            plan.samplers
                .iter()
                .all(|sampler| sampler.time_filter.is_none())
        );
        assert!(plan.check_since().is_ok());
    }

    #[test]
    fn requires_since_when_a_sampler_has_a_time_column() {
        let error = plan(TIMED).unwrap().check_since().unwrap_err().to_string();
        assert!(error.contains("2 sampler(s) have a time_column"), "{error}");
        assert!(error.contains("config sampler #2 (users)"), "{error}");
        assert!(error.contains("--since ever"), "{error}");

        assert!(plan(SHOP).unwrap().check_since().is_ok());
    }

    #[test]
    fn describes_the_plan() {
        let described = plan_since(TIMED, "2 weeks").describe();
        assert!(
            described.contains("3 tables, 3 samplers, 2 with a time_column"),
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

        let unset = plan(TIMED).unwrap().describe();
        assert!(unset.contains("a run would refuse to start"), "{unset}");
    }

    const SHOP: &str = r#"
samplers:
  - table: countries
  - table: users
    order_by: id DESC
    limit: 100
    of:
      country_id: countries.id
  - table: users
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
    fn orders_parents_before_children() {
        let plan = plan(SHOP).unwrap();
        let position = |table: &str| plan.order.iter().position(|t| t == table).unwrap();

        assert_eq!(plan.order.len(), 5);
        assert!(position("countries") < position("users"));
        assert!(position("users") < position("phones"));
        assert!(position("users") < position("orders"));
        assert!(position("orders") < position("line_items"));
    }

    #[test]
    fn tracks_kept_columns_full_tables_and_dedupe() {
        let plan = plan(SHOP).unwrap();

        assert_eq!(
            plan.tables["users"].keep,
            BTreeSet::from(["id".to_string()])
        );
        assert_eq!(
            plan.tables["orders"].keep,
            BTreeSet::from(["id".to_string()])
        );
        assert!(plan.tables["phones"].keep.is_empty());

        assert!(plan.tables["countries"].full);
        assert!(!plan.tables["users"].full);

        assert!(plan.tables["users"].dedupe);
        assert!(!plan.tables["orders"].dedupe);
    }

    #[test]
    fn rejects_missing_parents() {
        let error =
            plan("samplers:\n  - table: phones\n    of:\n      user_id: users.id\n").unwrap_err();
        assert!(
            error.to_string().contains("no sampler extracts users"),
            "{error}"
        );
    }

    const PROJECTS: &str = r#"
samplers:
  - table: users
  - table: project_tasks
    time_column: updated_at
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
    fn groups_tables_that_depend_on_each_other() {
        let plan = plan_since(PROJECTS, "2 weeks");
        let position = |table: &str| plan.order.iter().position(|t| t == table).unwrap();

        assert_eq!(plan.cycles, [["project_tasks", "projects"]]);
        assert_eq!(plan.tables["projects"].cycle, Some(0));
        assert_eq!(plan.tables["project_tasks"].cycle, Some(0));
        assert_eq!(plan.tables["users"].cycle, None);
        assert!(plan.tables["project_tasks"].dedupe);

        assert!(position("users") < position("projects"));
        assert!(position("project_tasks") < position("task_notes"));
        assert!(position("projects") < position("task_notes"));
    }

    #[test]
    fn knows_which_samplers_run_in_rounds() {
        let plan = plan_since(PROJECTS, "2 weeks");
        assert!(!plan.loops(1), "the timed project_tasks sampler is a seed");
        assert!(plan.loops(2));
        assert!(plan.loops(3));
        assert!(!plan.loops(4), "task_notes isn't in the cycle");
        assert_eq!(plan.cycle_deps(5), ["id"], "users.id is outside the cycle");
    }

    #[test]
    fn a_cycle_needs_no_rounds_when_since_keeps_everything() {
        // The timed project_tasks sampler keeps every row, so deps on project_tasks need no values.
        let plan = plan_since(PROJECTS, "ever");
        assert!(plan.tables["project_tasks"].full);
        assert!(!plan.loops(2));
        assert!(plan.loops(3), "projects still isn't full");
    }

    #[test]
    fn plans_self_references() {
        let plan = plan(
            r#"
samplers:
  - table: employees
    limit: 10
  - table: employees
    of:
      id: employees.manager_id
"#,
        )
        .unwrap();
        assert_eq!(plan.cycles, [["employees"]]);
        assert!(!plan.loops(0));
        assert!(plan.loops(1));
    }

    #[test]
    fn a_dep_on_a_full_table_in_the_cycle_does_not_loop() {
        let plan = plan(
            r#"
samplers:
  - table: categories
  - table: categories
    of:
      parent_id: categories.id
"#,
        )
        .unwrap();
        assert_eq!(plan.cycles, [["categories"]]);
        assert!(!plan.loops(1));
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
        .unwrap_err();
        assert!(error.to_string().contains("limit can't be used"), "{error}");
        assert!(error.to_string().contains("sampler #2"), "{error}");
    }

    #[test]
    fn describes_cycles() {
        let described = plan_since(PROJECTS, "2 weeks").describe();
        assert!(
            described.contains("projects  (cycle: project_tasks, projects;"),
            "{described}"
        );
        assert!(
            described.contains("sampler #3 (projects)  (in rounds)"),
            "{described}"
        );
    }

    const WRITE_OFFS: &str = r#"
samplers:
  - table: written_off_projects
  - table: written_off_projects
    name: recent_write_offs
    time_column: updated_at
  - table: projects
    of:
      id: recent_write_offs.project_id
"#;

    #[test]
    fn deps_can_name_a_sampler() {
        let plan = plan_since(WRITE_OFFS, "2 weeks");
        assert_eq!(plan.named["recent_write_offs"], 1);
        assert_eq!(
            plan.named_keep["recent_write_offs"],
            BTreeSet::from(["project_id".to_string()])
        );
        assert!(plan.tables["written_off_projects"].keep.is_empty());
        assert!(plan.tables["written_off_projects"].full);
        assert_eq!(
            plan.tables["projects"].parents,
            BTreeSet::from(["written_off_projects".to_string()])
        );

        // The table keeps every row, but the named sampler doesn't, so its values matter.
        let parent = &plan.samplers[2].of["id"];
        assert_eq!(plan.parent_table(parent), "written_off_projects");
        assert!(!plan.allows_any(parent));
        assert!(
            plan.describe()
                .contains("`id` IN (<recent_write_offs.project_id>)"),
            "{}",
            plan.describe()
        );
    }

    #[test]
    fn a_named_sampler_that_keeps_everything_allows_any_value() {
        let plan = plan_since(WRITE_OFFS, "ever");
        assert!(plan.allows_any(&plan.samplers[2].of["id"]));
    }

    #[test]
    fn rejects_bad_names() {
        let clash = "samplers:\n  - table: users\n  - table: orders\n    name: users\n";
        let error = plan(clash).unwrap_err().to_string();
        assert!(error.contains("name `users` is also a table"), "{error}");

        let twice = "samplers:\n  - table: users\n    name: a\n  - table: orders\n    name: a\n";
        let error = plan(twice).unwrap_err().to_string();
        assert!(error.contains("name `a` is already used"), "{error}");
    }

    /// The example from the design discussion, as written.
    const REFERENCES: &str = r#"
samplers:
  - table: users
    deps:
      territory_id: territories.id
    where: age > 25

  - table: orders
    deps:
      user_id: users.id
    time_column: created_at

  - table: territories
"#;

    #[test]
    fn deps_pull_in_the_rows_they_point_at() {
        let plan = plan_since(REFERENCES, "2 weeks");

        // users: its own sampler, plus one for the users the orders point at.
        let users: Vec<&Sampler> = plan.tables["users"]
            .samplers
            .iter()
            .map(|&i| &plan.samplers[i])
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
        let orders = &plan.samplers[plan.tables["orders"].samplers[0]];
        assert!(orders.of.is_empty());
        assert!(orders.time_filter.is_some());
        assert_eq!(
            plan.tables["orders"].keep,
            BTreeSet::from(["user_id".to_string()])
        );
        let position = |table: &str| plan.order.iter().position(|t| t == table).unwrap();
        assert!(position("orders") < position("users"));

        // territories keeps every row, so nothing needs pulling there.
        assert_eq!(plan.tables["territories"].samplers.len(), 1);
        assert!(plan.cycles.is_empty());
    }

    #[test]
    fn a_referenced_table_needs_no_sampler_of_its_own() {
        let plan = plan_since(
            "samplers:\n  - table: orders\n    time_column: created_at\n    deps:\n      user_id: users.id\n",
            "1 week",
        );
        assert_eq!(plan.tables["users"].samplers.len(), 1);
        assert!(!plan.tables["users"].full);
    }

    #[test]
    fn deps_on_a_table_that_points_at_itself_form_a_cycle() {
        let plan = plan(
            "samplers:\n  - table: employees\n    where: id = 40\n    deps:\n      manager_id: employees.id\n",
        )
        .unwrap();
        assert_eq!(plan.cycles, [["employees"]]);
        assert!(
            plan.loops(1),
            "the pull follows managers of pulled managers"
        );
    }

    #[test]
    fn rejects_deps_on_sampler_names_and_conflicting_deps() {
        let named = "samplers:\n  - table: users\n    name: adults\n  - table: orders\n    deps:\n      user_id: adults.id\n";
        let error = plan(named).unwrap_err().to_string();
        assert!(error.contains("Use `of`"), "{error}");

        let conflict = "samplers:\n  - table: orders\n    deps:\n      user_id: users.id\n  - table: orders\n    where: total > 0\n    deps:\n      user_id: people.id\n";
        let error = plan(conflict).unwrap_err().to_string();
        assert!(
            error.contains("orders.user_id points at people.id"),
            "{error}"
        );
    }
}
