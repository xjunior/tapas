//! Runs a plan's samplers against the source and streams their rows out as INSERT statements.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexSet;
use mysql_async::{Pool, Row, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use xxhash_rust::xxh3;

use crate::plan::Plan;
use crate::sql::{self, Allowed, Dep, Kind};

/// How the extraction splits its queries and statements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Each `IN (...)` list holds at most this many values. It must be at least 1.
    pub chunk_size: usize,
    /// Each INSERT statement holds at most this many rows.
    pub batch_rows: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_size: 5_000,
            batch_rows: 500,
        }
    }
}

/// The distinct values of one kept column, in the order they were found, so a cycle can ask for
/// just the values found since its last round.
#[derive(Debug, Clone, Default)]
struct KeySet {
    // One copy of each value: an `IndexSet` both dedupes and keeps the order.
    values: IndexSet<Vec<u8>>,
}

impl KeySet {
    fn insert(&mut self, value: &[u8]) {
        if !self.values.contains(value) {
            self.values.insert(value.to_vec());
        }
    }

    /// Returns how many distinct values were found so far.
    #[must_use]
    fn found(&self) -> usize {
        self.values.len()
    }

    /// Returns the values found after the first `from`, in the order they were found.
    #[must_use]
    fn values_from(&self, from: usize) -> Vec<Vec<u8>> {
        self.values.as_slice()[from..].iter().cloned().collect()
    }
}

#[derive(Debug, Clone, Default)]
struct TableState {
    /// Literals of the values extracted so far, per kept column.
    keys: BTreeMap<String, KeySet>,
    /// Hashes of rows written so far, for tables with more than one sampler.
    seen: HashSet<u128>,
    rows: u64,
}

impl TableState {
    /// Starts with no rows, keeping the values of `columns`.
    #[must_use]
    fn new_with_keys(columns: &BTreeSet<String>) -> Self {
        let keys: BTreeMap<String, KeySet> = columns
            .iter()
            .map(|column| (column.clone(), KeySet::default()))
            .collect();
        Self {
            keys,
            ..Self::default()
        }
    }
}

fn lock(state: &Mutex<TableState>) -> MutexGuard<'_, TableState> {
    state
        .lock()
        .expect("no task panics while it holds a table's state")
}

// Shared through an `Arc` because every sampler and query runs in its own spawned task, which
// can't borrow from `extract`.
struct Shared {
    plan: Plan,
    pool: Pool,
    options: Options,
    output: mpsc::Sender<Vec<u8>>,
    tables: HashMap<String, Mutex<TableState>>,
    /// Keys of named samplers' own rows; only `keys` is used.
    named: HashMap<String, Mutex<TableState>>,
    started: Instant,
}

/// Extracts every sampler's rows, sending INSERT statements to `output`. Returns the number of
/// rows written per table.
///
/// # Panics
///
/// Panics if `options.chunk_size` is 0.
pub async fn extract(
    plan: Plan,
    pool: Pool,
    options: Options,
    output: mpsc::Sender<Vec<u8>>,
) -> Result<BTreeMap<String, u64>> {
    let tables: HashMap<String, Mutex<TableState>> = plan
        .tables
        .iter()
        .map(|(name, table)| {
            let state = TableState::new_with_keys(&table.keep);
            (name.clone(), Mutex::new(state))
        })
        .collect();
    let named: HashMap<String, Mutex<TableState>> = plan
        .named_keep
        .iter()
        .map(|(name, columns)| {
            let state = TableState::new_with_keys(columns);
            (name.clone(), Mutex::new(state))
        })
        .collect();
    let context = Arc::new(Shared {
        plan,
        named,
        pool,
        options,
        output,
        tables,
        started: Instant::now(),
    });

    let mut samplers_left: HashMap<&str, usize> = context
        .plan
        .tables
        .iter()
        .map(|(name, table)| (name.as_str(), table.samplers.len()))
        .collect();
    let mut done: HashSet<&str> = HashSet::new();
    let mut started = Started {
        samplers: vec![false; context.plan.samplers.len()],
        cycles: vec![false; context.plan.cycles.len()],
    };
    let mut running = JoinSet::new();

    start_ready(&context, &done, &mut started, &mut running);

    while let Some(finished) = running.join_next().await {
        let finished_tables: Vec<&str> = match finished.context("extraction task panicked")?? {
            Unit::Sampler(index) => {
                let table = context.plan.samplers[index].table.as_str();
                let left = samplers_left
                    .get_mut(table)
                    .expect("every sampler's table has a count of samplers left");
                *left -= 1;
                if *left > 0 {
                    continue;
                }
                vec![table]
            }
            Unit::Cycle(cycle) => context.plan.cycles[cycle]
                .iter()
                .map(String::as_str)
                .collect(),
        };
        for table in finished_tables {
            let rows = lock(&context.tables[table]).rows;
            log(&context, &format!("{table}: done, {rows} rows"));
            done.insert(table);
        }
        start_ready(&context, &done, &mut started, &mut running);
    }

    let mut rows = BTreeMap::new();
    for (name, state) in &context.tables {
        rows.insert(name.clone(), lock(state).rows);
    }
    Ok(rows)
}

/// What a finished task extracted: one sampler, or a whole cycle of tables.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Unit {
    /// The sampler at this index of `Plan::samplers`.
    Sampler(usize),
    /// The cycle at this index of `Plan::cycles`.
    Cycle(usize),
}

/// Which samplers and cycles have a task, by index into `Plan::samplers` and `Plan::cycles`.
#[derive(Debug, Clone)]
struct Started {
    samplers: Vec<bool>,
    cycles: Vec<bool>,
}

fn start_ready(
    context: &Arc<Shared>,
    done: &HashSet<&str>,
    started: &mut Started,
    running: &mut JoinSet<Result<Unit>>,
) {
    let plan = &context.plan;

    for (index, sampler) in plan.samplers.iter().enumerate() {
        if started.samplers[index]
            || plan.tables[&sampler.table].cycle.is_some()
            || !sampler
                .of
                .values()
                .all(|parent| done.contains(plan.parent_table(parent)))
        {
            continue;
        }
        started.samplers[index] = true;
        let context = Arc::clone(context);
        running.spawn(async move {
            let deps = current_deps(&context, index, None);
            run_sampler(&context, index, deps)
                .await
                .with_context(|| context.plan.samplers[index].origin.clone())?;
            Ok(Unit::Sampler(index))
        });
    }

    for (cycle, members) in plan.cycles.iter().enumerate() {
        let ready = members.iter().all(|table| {
            plan.tables[table]
                .parents
                .iter()
                .all(|parent| members.contains(parent) || done.contains(parent.as_str()))
        });
        if started.cycles[cycle] || !ready {
            continue;
        }
        started.cycles[cycle] = true;
        let context = Arc::clone(context);
        running.spawn(async move {
            run_cycle(&context, cycle)
                .await
                .with_context(|| format!("cycle {}", context.plan.cycles[cycle].join(", ")))?;
            Ok(Unit::Cycle(cycle))
        });
    }
}

/// Extracts a group of tables that depend on each other. Samplers with no dep inside the cycle
/// run once; the rest run in rounds, each round using only the values found since the last one,
/// until a round writes no new rows.
async fn run_cycle(context: &Arc<Shared>, cycle: usize) -> Result<()> {
    let plan = &context.plan;
    let members = &plan.cycles[cycle];
    let samplers: Vec<usize> = members
        .iter()
        .flat_map(|table| plan.tables[table].samplers.iter().copied())
        .collect();
    let (looping, seeds): (Vec<usize>, Vec<usize>) =
        samplers.into_iter().partition(|&index| plan.loops(index));

    let mut running = JoinSet::new();
    for index in seeds {
        let context = Arc::clone(context);
        running.spawn(async move {
            let deps = current_deps(&context, index, None);
            run_sampler(&context, index, deps)
                .await
                .with_context(|| context.plan.samplers[index].origin.clone())
        });
    }
    while let Some(finished) = running.join_next().await {
        finished.context("sampler task panicked")??;
    }

    // Per looping sampler: how many values of its cycle dep it has already used.
    let mut cursors: HashMap<usize, usize> = HashMap::new();
    let mut round = 0;
    loop {
        round += 1;
        let before = cycle_rows(context, members);

        let mut running = JoinSet::new();
        for &index in &looping {
            let deps = current_deps(context, index, Some(&mut cursors));
            let context = Arc::clone(context);
            running.spawn(async move {
                run_sampler(&context, index, deps)
                    .await
                    .with_context(|| context.plan.samplers[index].origin.clone())
            });
        }
        while let Some(finished) = running.join_next().await {
            finished.context("sampler task panicked")??;
        }

        let added = cycle_rows(context, members) - before;
        log(
            context,
            &format!(
                "cycle {}: round {round} added {added} rows",
                members.join(", ")
            ),
        );
        if added == 0 {
            return Ok(());
        }
    }
}

#[must_use]
fn cycle_rows(context: &Shared, members: &[String]) -> u64 {
    members
        .iter()
        .map(|table| lock(&context.tables[table]).rows)
        .sum()
}

/// Returns the values each dep of sampler `index` allows right now. With `cursors`, a sampler
/// with a single dep inside its cycle only gets the values found since its previous round; one
/// with several gets them all, since a new row can come from any combination.
#[must_use]
fn current_deps(
    context: &Shared,
    index: usize,
    cursors: Option<&mut HashMap<usize, usize>>,
) -> Vec<Dep> {
    let plan = &context.plan;
    let sampler = &plan.samplers[index];
    let cycle_deps = plan.cycle_deps(index);
    let mut cursors = cursors;

    sampler
        .of
        .iter()
        .map(|(column, parent)| {
            let column = column.clone();
            if plan.allows_any(parent) {
                return Dep {
                    column,
                    allowed: Allowed::AnyValue,
                };
            }
            let state = match context.named.get(&parent.table) {
                Some(named) => lock(named),
                None => lock(&context.tables[&parent.table]),
            };
            let keys = &state.keys[&parent.column];
            let from = match cursors.as_deref_mut() {
                Some(cursors) if cycle_deps == [column.as_str()] => {
                    let cursor = cursors.entry(index).or_insert(0);
                    mem::replace(cursor, keys.found())
                }
                _ => 0,
            };
            Dep {
                column,
                allowed: Allowed::Values(keys.values_from(from)),
            }
        })
        .collect()
}

async fn run_sampler(context: &Arc<Shared>, index: usize, deps: Vec<Dep>) -> Result<()> {
    let sampler = &context.plan.samplers[index];
    let time_filter = context.plan.time_filter(index);
    let queries = sql::sampler_queries(
        sampler,
        time_filter.as_deref(),
        &deps,
        context.options.chunk_size,
    );
    if queries.is_empty() {
        return Ok(());
    }
    log(
        context,
        &format!("{}: running {} queries", sampler.origin, queries.len()),
    );

    if let Some(limit) = sampler.limit {
        // Chunks run one after another so the limit holds across all of them.
        let mut remaining = limit;
        for mut query in queries {
            if remaining == 0 {
                break;
            }
            query.extend_from_slice(format!(" LIMIT {remaining}").as_bytes());
            remaining -= run_query(context, index, query).await?.min(remaining);
        }
    } else {
        let mut running = JoinSet::new();
        for query in queries {
            let context = Arc::clone(context);
            running.spawn(async move { run_query(&context, index, query).await });
        }
        while let Some(finished) = running.join_next().await {
            finished.context("query task panicked")??;
        }
    }
    Ok(())
}

/// Runs one SELECT, streaming its rows out as INSERTs. Returns the number of rows it returned.
async fn run_query(context: &Shared, index: usize, query: Vec<u8>) -> Result<u64> {
    use futures_util::StreamExt;
    use mysql_async::prelude::Queryable;

    let sampler = &context.plan.samplers[index];
    let table = sampler.table.as_str();
    let mut connection = context.pool.get_conn().await?;
    let mut result = connection
        .query_iter(query.as_slice())
        .await
        .with_context(|| format!("running {}", String::from_utf8_lossy(&query)))?;
    let Some(mut stream) = result.stream::<Row>().await? else {
        return Ok(0);
    };

    let columns = stream.columns();
    let names: Vec<String> = columns
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect();
    let kinds: Vec<Kind> = columns
        .iter()
        .map(|column| Kind::new(column.column_type(), column.character_set()))
        .collect();
    let mut named = None;
    if let Some(name) = &sampler.name
        && let Some(columns) = context.plan.named_keep.get(name)
    {
        named = Some(NamedKeep {
            name,
            columns: kept_columns(columns, &names, table)?,
        });
    }
    let keep = Keep {
        table: kept_columns(&context.plan.tables[table].keep, &names, table)?,
        named,
    };

    let mut returned = 0;
    let mut batch = Vec::with_capacity(context.options.batch_rows);
    while let Some(row) = stream.next().await {
        let row = row?;
        let mut literals = Vec::with_capacity(kinds.len());
        for (index, kind) in kinds.iter().enumerate() {
            let value = match row.as_ref(index) {
                Some(Value::NULL) | None => None,
                Some(Value::Bytes(bytes)) => Some(bytes.as_slice()),
                Some(
                    other @ (Value::Int(_)
                    | Value::UInt(_)
                    | Value::Float(_)
                    | Value::Double(_)
                    | Value::Date(..)
                    | Value::Time(..)),
                ) => bail!("unexpected value {other:?} in {table}.{}", names[index]),
            };
            literals.push(sql::literal(*kind, value));
        }
        batch.push(literals);
        returned += 1;

        if batch.len() >= context.options.batch_rows {
            let full = mem::replace(&mut batch, Vec::with_capacity(context.options.batch_rows));
            write_batch(context, table, &names, &keep, full).await?;
        }
    }
    write_batch(context, table, &names, &keep, batch).await?;
    Ok(returned)
}

/// A column of a query's rows whose values are remembered.
#[derive(Debug, Clone, Copy, PartialEq)]
struct KeptColumn<'plan> {
    name: &'plan str,
    /// Where the column sits in the query's rows.
    position: usize,
}

/// The columns of a query's rows to remember for the sampler's own name.
#[derive(Debug, Clone, PartialEq)]
struct NamedKeep<'plan> {
    name: &'plan str,
    columns: Vec<KeptColumn<'plan>>,
}

/// Which columns of a query's rows to remember: for the table, and for the sampler's own name.
#[derive(Debug, Clone, PartialEq)]
struct Keep<'plan> {
    table: Vec<KeptColumn<'plan>>,
    named: Option<NamedKeep<'plan>>,
}

/// Finds where each of `columns` sits among a query's column `names`. Fails if `table` lacks one.
fn kept_columns<'plan>(
    columns: &'plan BTreeSet<String>,
    names: &[String],
    table: &str,
) -> Result<Vec<KeptColumn<'plan>>> {
    let mut kept = Vec::with_capacity(columns.len());
    for name in columns {
        let position = names
            .iter()
            .position(|found| found == name)
            .ok_or_else(|| {
                anyhow!("{table} has no column {name}, which another sampler depends on")
            })?;
        kept.push(KeptColumn { name, position });
    }
    Ok(kept)
}

async fn write_batch(
    context: &Shared,
    table: &str,
    names: &[String],
    keep: &Keep<'_>,
    batch: Vec<Vec<Vec<u8>>>,
) -> Result<()> {
    // A named sampler's keys come from every row it returned, even rows another sampler of the
    // table already wrote.
    if let Some(named) = &keep.named {
        let mut state = lock(&context.named[named.name]);
        for row in &batch {
            for column in &named.columns {
                if row[column.position] != sql::NULL {
                    state
                        .keys
                        .get_mut(column.name)
                        .expect("a named sampler's state has a key set per kept column")
                        .insert(&row[column.position]);
                }
            }
        }
    }

    let rows = {
        let dedupe = context.plan.tables[table].dedupe;
        let mut state = lock(&context.tables[table]);
        let mut rows = Vec::with_capacity(batch.len());
        for row in batch {
            if dedupe && !state.seen.insert(row_hash(&row)) {
                continue;
            }
            for column in &keep.table {
                if row[column.position] != sql::NULL {
                    state
                        .keys
                        .get_mut(column.name)
                        .expect("a table's state has a key set per kept column")
                        .insert(&row[column.position]);
                }
            }
            rows.push(row);
        }
        state.rows += rows.len() as u64;
        rows
    };

    if !rows.is_empty() {
        context
            .output
            .send(sql::insert(table, names, &rows))
            .await
            .map_err(|_| anyhow!("the SQL writer stopped"))?;
    }
    Ok(())
}

#[must_use]
fn row_hash(row: &[Vec<u8>]) -> u128 {
    // Each value's length goes first, so moving a byte between columns changes the hash.
    let mut hasher = xxh3::Xxh3::new();
    for literal in row {
        hasher.update(&(literal.len() as u64).to_le_bytes());
        hasher.update(literal);
    }
    hasher.digest128()
}

fn log(context: &Shared, message: &str) {
    eprintln!(
        "[{:>7.2}s] {message}",
        context.started.elapsed().as_secs_f64()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[&[u8]]) -> Vec<Vec<u8>> {
        values.iter().map(|value| value.to_vec()).collect()
    }

    #[test]
    fn key_sets_keep_each_value_once_in_the_order_found() {
        let mut keys = KeySet::default();
        for value in [b"3", b"1", b"3", b"2", b"1"] {
            keys.insert(value);
        }
        assert_eq!(keys.found(), 3);
        assert_eq!(keys.values_from(0), row(&[b"3", b"1", b"2"]));
        assert_eq!(keys.values_from(1), row(&[b"1", b"2"]));
        assert_eq!(keys.values_from(3), row(&[]));
    }

    #[test]
    fn row_hashes_match_for_equal_rows() {
        assert_eq!(
            row_hash(&row(&[b"1", b"'a'"])),
            row_hash(&row(&[b"1", b"'a'"]))
        );
    }

    #[test]
    fn row_hashes_tell_where_each_value_ends() {
        assert_ne!(
            row_hash(&row(&[b"ab", b"c"])),
            row_hash(&row(&[b"a", b"bc"])),
            "moving a byte between columns makes another row"
        );
        assert_ne!(row_hash(&row(&[b"a", b""])), row_hash(&row(&[b"a"])));
    }
}
