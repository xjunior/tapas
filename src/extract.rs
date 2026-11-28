//! Runs a plan's samplers against the source and streams their rows out as INSERT statements.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use indexmap::IndexSet;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use xxhash_rust::xxh3;

use crate::config::Columns;
use crate::plan::Plan;
use crate::sanitize::{Placed, Sanitizer, Scratch, Seed};
use crate::source::{RowSink, Source};
use crate::sql::{self, Allowed, Dep, Kind};

/// How the extraction runs: how it splits its queries and statements, and where fake values
/// start from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Each `IN (...)` list holds at most this many values. It must be at least 1.
    pub chunk_size: usize,
    /// Each INSERT statement holds at most this many rows.
    pub batch_rows: usize,
    /// Where fake values start from. Without one, each run picks its own.
    pub sanitize_seed: Option<Seed>,
    /// Errors show the values they're about, such as every row of a statement the target
    /// refused. Off by default, so errors that end up in logs hold no data.
    pub debug: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_size: 5_000,
            batch_rows: 500,
            sanitize_seed: None,
            debug: false,
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
    keys: BTreeMap<Columns, KeySet>,
    /// Hashes of rows written so far, for tables with more than one sampler.
    seen: HashSet<u128>,
    rows: u64,
}

impl TableState {
    /// Starts with no rows, keeping the values of `columns`.
    #[must_use]
    fn new_with_keys(columns: &BTreeSet<Columns>) -> Self {
        let keys: BTreeMap<Columns, KeySet> = columns
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
    source: Source,
    options: Options,
    output: mpsc::Sender<Vec<u8>>,
    sanitizer: Sanitizer,
    tables: HashMap<String, Mutex<TableState>>,
    /// Keys of named samplers' own rows; only `keys` is used.
    named: HashMap<String, Mutex<TableState>>,
    started: Instant,
}

/// Extracts every sampler's rows, sending INSERT statements to `output` with the values
/// `sanitizer` covers replaced. Returns the number of rows written per table.
///
/// # Panics
///
/// Panics if `options.chunk_size` is 0.
pub async fn extract(
    plan: Plan,
    source: Source,
    options: Options,
    sanitizer: Sanitizer,
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
        source,
        options,
        output,
        sanitizer,
        tables,
        started: Instant::now(),
    });

    let mut samplers_left: HashMap<&str, usize> = context
        .plan
        .tables
        .iter()
        .map(|(name, table)| (name.as_str(), table.samplers.len()))
        .collect();
    // A table nothing extracts from has no rows to wait for, so its children can start at once.
    let mut done: HashSet<&str> = context
        .plan
        .tables
        .iter()
        .filter(|(_, table)| table.samplers.is_empty() && table.cycle.is_none())
        .map(|(name, _)| name.as_str())
        .collect();
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
            || !sampler.takes_rows()
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
        .map(|(columns, parent)| {
            let columns = columns.clone();
            if plan.allows_any(parent) {
                return Dep {
                    columns,
                    allowed: Allowed::AnyValue,
                };
            }
            let state = match context.named.get(&parent.table) {
                Some(named) => lock(named),
                None => lock(&context.tables[&parent.table]),
            };
            let keys = &state.keys[&parent.columns];
            let from = match cursors.as_deref_mut() {
                Some(cursors) if cycle_deps == [&columns] => {
                    let cursor = cursors.entry(index).or_insert(0);
                    mem::replace(cursor, keys.found())
                }
                _ => 0,
            };
            Dep {
                columns,
                allowed: Allowed::Values(keys.values_from(from)),
            }
        })
        .collect()
}

async fn run_sampler(context: &Arc<Shared>, index: usize, deps: Vec<Dep>) -> Result<()> {
    let sampler = &context.plan.samplers[index];
    let dialect = context.source.dialect();
    let time_filter = context.plan.time_filter(index, dialect);
    let queries = sql::sampler_queries(
        dialect,
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

    if let Some(limit) = sampler.row_cap() {
        // Chunks run one after another so the limit holds across all of them.
        let mut remaining = limit;
        for query in queries {
            if remaining == 0 {
                break;
            }
            remaining -= run_query(context, index, query, Some(remaining))
                .await?
                .min(remaining);
        }
    } else {
        let mut running = JoinSet::new();
        for query in queries {
            let context = Arc::clone(context);
            running.spawn(async move { run_query(&context, index, query, None).await });
        }
        while let Some(finished) = running.join_next().await {
            finished.context("query task panicked")??;
        }
    }
    Ok(())
}

/// Runs one SELECT of sampler `index`, up to `limit` rows, streaming its rows out as INSERTs.
/// Returns the number of rows it returned.
async fn run_query(
    context: &Shared,
    index: usize,
    mut query: Vec<u8>,
    limit: Option<u64>,
) -> Result<u64> {
    push_limit(&mut query, limit);
    let table = context.plan.samplers[index].table.as_str();
    let batcher = context
        .source
        .query(table, &query, |names, kinds| {
            Batcher::try_new(context, index, names, kinds)
        })
        .await;
    let ran = match batcher {
        Ok(Some(batcher)) => batcher.finish().await,
        Ok(None) => Ok(0),
        Err(error) => Err(error),
    };
    ran.with_context(|| format!("running {}", shown_query(context, index, &query, limit)))
}

/// A query of sampler `index` as its errors show it: in full with `debug`, or else as its
/// outline, which writes each list of values as the column they come from.
fn shown_query(context: &Shared, index: usize, query: &[u8], limit: Option<u64>) -> String {
    if context.options.debug {
        return String::from_utf8_lossy(query).into_owned();
    }
    let mut outline = context.plan.query_outline(index, context.source.dialect());
    push_limit(&mut outline, limit);
    String::from_utf8_lossy(&outline).into_owned()
}

fn push_limit(query: &mut Vec<u8>, limit: Option<u64>) {
    if let Some(limit) = limit {
        query.extend_from_slice(format!(" LIMIT {limit}").as_bytes());
    }
}

/// Gathers one query's rows, as literals, into batches, and writes each batch out once it's full.
struct Batcher<'query> {
    context: &'query Shared,
    table: &'query str,
    names: Vec<String>,
    keep: Keep<'query>,
    sanitize: Placed<'query>,
    batch: Vec<Vec<Vec<u8>>>,
    returned: u64,
}

impl<'query> Batcher<'query> {
    /// Starts on the rows of a query of sampler `index`, whose columns are `names`, with values
    /// written as `kinds`. Fails if the query lacks a column that other samplers or a sanitize
    /// rule need.
    fn try_new(
        context: &'query Shared,
        index: usize,
        names: &[String],
        kinds: &[Kind],
    ) -> Result<Self> {
        let sampler = &context.plan.samplers[index];
        let table = sampler.table.as_str();
        let mut named = None;
        if let Some(name) = &sampler.name
            && let Some(columns) = context.plan.named_keep.get(name)
        {
            named = Some(NamedKeep {
                name,
                columns: kept_columns(columns, names, table)?,
            });
        }
        let keep = Keep {
            table: kept_columns(&context.plan.tables[table].keep, names, table)?,
            named,
        };
        let sanitize = context.sanitizer.place(table, names, kinds)?;
        Ok(Self {
            context,
            table,
            names: names.to_vec(),
            keep,
            sanitize,
            batch: Vec::with_capacity(context.options.batch_rows),
            returned: 0,
        })
    }

    /// Adds a row of literals, and writes the batch out once it's full.
    async fn push_row(&mut self, row: Vec<Vec<u8>>) -> Result<()> {
        self.batch.push(row);
        self.returned += 1;
        if self.batch.len() >= self.context.options.batch_rows {
            let capacity = self.context.options.batch_rows;
            let full = mem::replace(&mut self.batch, Vec::with_capacity(capacity));
            self.write(full).await?;
        }
        Ok(())
    }

    /// Writes out the last batch. Returns the number of rows the query returned.
    async fn finish(mut self) -> Result<u64> {
        let last = mem::take(&mut self.batch);
        self.write(last).await?;
        Ok(self.returned)
    }

    async fn write(&self, batch: Vec<Vec<Vec<u8>>>) -> Result<()> {
        write_batch(
            self.context,
            self.table,
            &self.names,
            &self.keep,
            &self.sanitize,
            batch,
        )
        .await
    }
}

impl RowSink for Batcher<'_> {
    async fn push(&mut self, row: Vec<Vec<u8>>) -> Result<()> {
        self.push_row(row).await
    }
}

/// The columns of a reference whose values are remembered.
#[derive(Debug, Clone, PartialEq)]
struct KeptColumn<'plan> {
    columns: &'plan Columns,
    /// Where each of the columns sits in the query's rows.
    positions: Vec<usize>,
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
    kept_columns: &'plan BTreeSet<Columns>,
    names: &[String],
    table: &str,
) -> Result<Vec<KeptColumn<'plan>>> {
    let mut kept = Vec::with_capacity(kept_columns.len());
    for columns in kept_columns {
        let mut positions = Vec::with_capacity(columns.as_slice().len());
        for name in columns.iter() {
            let position = names
                .iter()
                .position(|found| found == name)
                .ok_or_else(|| {
                    anyhow!("{table} has no column {name}, which another sampler depends on")
                })?;
            positions.push(position);
        }
        kept.push(KeptColumn { columns, positions });
    }
    Ok(kept)
}

async fn write_batch(
    context: &Shared,
    table: &str,
    names: &[String],
    keep: &Keep<'_>,
    sanitize: &Placed<'_>,
    batch: Vec<Vec<Vec<u8>>>,
) -> Result<()> {
    // Reused for every composite key of the batch.
    let mut tuple: Vec<u8> = Vec::new();

    // A named sampler's keys come from every row it returned, even rows another sampler of the
    // table already wrote.
    if let Some(named) = &keep.named {
        let mut state = lock(&context.named[named.name]);
        for row in &batch {
            for column in &named.columns {
                let keys = state
                    .keys
                    .get_mut(column.columns)
                    .expect("a named sampler's state has a key set per kept column");
                remember(keys, row, &column.positions, &mut tuple);
            }
        }
    }

    let mut rows = {
        let dedupe = context.plan.tables[table].dedupe;
        let mut state = lock(&context.tables[table]);
        let mut rows = Vec::with_capacity(batch.len());
        for row in batch {
            if dedupe && !state.seen.insert(row_hash(&row)) {
                continue;
            }
            for column in &keep.table {
                let keys = state
                    .keys
                    .get_mut(column.columns)
                    .expect("a table's state has a key set per kept column");
                remember(keys, &row, &column.positions, &mut tuple);
            }
            rows.push(row);
        }
        state.rows += rows.len() as u64;
        rows
    };

    // After dedupe and keys, which need the original values, and only for rows that are written.
    if !sanitize.is_empty() {
        let mut scratch = Scratch::default();
        for row in &mut rows {
            sanitize.apply(row, &mut scratch);
        }
    }

    if !rows.is_empty() {
        context
            .output
            .send(sql::insert(context.source.dialect(), table, names, &rows))
            .await
            .map_err(|_| anyhow!("the output stopped taking statements"))?;
    }
    Ok(())
}

/// Remembers the value `row` holds in a kept column in `keys`: its literal, or for a composite
/// reference the literals of its columns as a tuple, such as `(1, 2)`, which `IN` takes as it is.
/// A NULL in any of the columns points at nothing, so nothing is remembered. `tuple` is scratch
/// space, reused from row to row.
fn remember(keys: &mut KeySet, row: &[Vec<u8>], positions: &[usize], tuple: &mut Vec<u8>) {
    if let [position] = positions {
        if row[*position] != sql::NULL {
            keys.insert(&row[*position]);
        }
        return;
    }
    tuple.clear();
    tuple.push(b'(');
    for (index, &position) in positions.iter().enumerate() {
        if row[position] == sql::NULL {
            return;
        }
        if index > 0 {
            tuple.extend_from_slice(b", ");
        }
        tuple.extend_from_slice(&row[position]);
    }
    tuple.push(b')');
    keys.insert(tuple);
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
    fn remembers_a_composite_key_as_one_tuple() {
        let row = row(&[b"7", b"1", b"NULL", b"'a'"]);
        let mut tuple = Vec::new();

        let mut keys = KeySet::default();
        remember(&mut keys, &row, &[1, 0], &mut tuple);
        remember(&mut keys, &row, &[0, 3], &mut tuple);
        assert_eq!(
            keys.values_from(0),
            [b"(1, 7)".to_vec(), b"(7, 'a')".to_vec()],
            "each key is its columns' literals in order"
        );

        let mut keys = KeySet::default();
        remember(&mut keys, &row, &[0, 2], &mut tuple);
        assert_eq!(keys.found(), 0, "a key with a NULL part points at nothing");

        remember(&mut keys, &row, &[1], &mut tuple);
        assert_eq!(
            keys.values_from(0),
            [b"1".to_vec()],
            "one column is remembered as it is"
        );
        remember(&mut keys, &row, &[2], &mut tuple);
        assert_eq!(keys.found(), 1, "a NULL is not remembered");
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
