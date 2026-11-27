use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{Pool, Row, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::xxh3_128;

use crate::plan::Plan;
use crate::sql::{self, Allowed, Kind};

#[derive(Debug, Clone)]
pub struct Options {
    /// Values per `IN (...)` list.
    pub chunk_size: usize,
    /// Rows per INSERT statement.
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
#[derive(Default)]
struct KeySet {
    seen: HashSet<Vec<u8>>,
    order: Vec<Vec<u8>>,
}

impl KeySet {
    fn insert(&mut self, value: &[u8]) {
        if !self.seen.contains(value) {
            self.seen.insert(value.to_vec());
            self.order.push(value.to_vec());
        }
    }
}

#[derive(Default)]
struct TableState {
    /// Kept column -> literals of the values extracted so far.
    keys: BTreeMap<String, KeySet>,
    /// Hashes of rows written so far, for tables with more than one sampler.
    seen: HashSet<u128>,
    rows: u64,
}

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
pub async fn extract(
    plan: Plan,
    pool: Pool,
    options: Options,
    output: mpsc::Sender<Vec<u8>>,
) -> Result<BTreeMap<String, u64>> {
    let tables = plan
        .tables
        .iter()
        .map(|(name, table)| {
            let keys = table
                .keep
                .iter()
                .map(|column| (column.clone(), KeySet::default()))
                .collect();
            (
                name.clone(),
                Mutex::new(TableState {
                    keys,
                    ..Default::default()
                }),
            )
        })
        .collect();
    let named = plan
        .named_keep
        .iter()
        .map(|(name, columns)| {
            let keys = columns
                .iter()
                .map(|column| (column.clone(), KeySet::default()))
                .collect();
            (
                name.clone(),
                Mutex::new(TableState {
                    keys,
                    ..Default::default()
                }),
            )
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
    let mut done: HashSet<String> = HashSet::new();
    let mut started = Started {
        samplers: vec![false; context.plan.samplers.len()],
        cycles: vec![false; context.plan.cycles.len()],
    };
    let mut running = JoinSet::new();

    start_ready(&context, &done, &mut started, &mut running);

    while let Some(finished) = running.join_next().await {
        let finished_tables: Vec<String> = match finished.context("extraction task panicked")?? {
            Unit::Sampler(index) => {
                let table = context.plan.samplers[index].table.as_str();
                let left = samplers_left.get_mut(table).unwrap();
                *left -= 1;
                if *left > 0 {
                    continue;
                }
                vec![table.to_string()]
            }
            Unit::Cycle(cycle) => context.plan.cycles[cycle].clone(),
        };
        for table in finished_tables {
            let rows = context.tables[&table].lock().unwrap().rows;
            log(&context, &format!("{table}: done, {rows} rows"));
            done.insert(table);
        }
        start_ready(&context, &done, &mut started, &mut running);
    }

    let mut rows = BTreeMap::new();
    for (name, state) in &context.tables {
        rows.insert(name.clone(), state.lock().unwrap().rows);
    }
    Ok(rows)
}

/// What a finished task extracted: one sampler, or a whole cycle of tables.
enum Unit {
    Sampler(usize),
    Cycle(usize),
}

struct Started {
    samplers: Vec<bool>,
    cycles: Vec<bool>,
}

fn start_ready(
    context: &Arc<Shared>,
    done: &HashSet<String>,
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
                .all(|parent| members.contains(parent) || done.contains(parent))
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

fn cycle_rows(context: &Shared, members: &[String]) -> u64 {
    members
        .iter()
        .map(|table| context.tables[table].lock().unwrap().rows)
        .sum()
}

/// The values each dep of sampler `index` allows right now. With `cursors`, a sampler with a
/// single dep inside its cycle only gets the values found since its previous round; one with
/// several gets them all, since a new row can come from any combination.
fn current_deps(
    context: &Shared,
    index: usize,
    cursors: Option<&mut HashMap<usize, usize>>,
) -> Vec<(String, Allowed)> {
    let plan = &context.plan;
    let sampler = &plan.samplers[index];
    let cycle_deps = plan.cycle_deps(index);
    let mut cursors = cursors;

    sampler
        .of
        .iter()
        .map(|(column, parent)| {
            if plan.allows_any(parent) {
                return (column.clone(), Allowed::AnyValue);
            }
            let state = match context.named.get(&parent.table) {
                Some(named) => named.lock().unwrap(),
                None => context.tables[&parent.table].lock().unwrap(),
            };
            let values = &state.keys[&parent.column].order;
            let from = match cursors.as_deref_mut() {
                Some(cursors) if cycle_deps == [column.as_str()] => {
                    let cursor = cursors.entry(index).or_insert(0);
                    std::mem::replace(cursor, values.len())
                }
                _ => 0,
            };
            (column.clone(), Allowed::Values(values[from..].to_vec()))
        })
        .collect()
}

async fn run_sampler(
    context: &Arc<Shared>,
    index: usize,
    deps: Vec<(String, Allowed)>,
) -> Result<()> {
    let sampler = &context.plan.samplers[index];
    let queries = sql::sampler_queries(sampler, &deps, context.options.chunk_size);
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
        .map(|column| Kind::of(column.column_type(), column.character_set()))
        .collect();
    let position = |column: &String| -> Result<(String, usize)> {
        let index = names
            .iter()
            .position(|name| name == column)
            .ok_or_else(|| {
                anyhow!("{table} has no column {column}, which another sampler depends on")
            })?;
        Ok((column.clone(), index))
    };
    let keep = Keep {
        table: context.plan.tables[table]
            .keep
            .iter()
            .map(position)
            .collect::<Result<_>>()?,
        named: match &sampler.name {
            Some(name) => context
                .plan
                .named_keep
                .get(name)
                .map(|columns| columns.iter().map(position).collect::<Result<Vec<_>>>())
                .transpose()?
                .map(|columns| (name.clone(), columns)),
            None => None,
        },
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
                Some(other) => bail!("unexpected value {other:?} in {table}.{}", names[index]),
            };
            literals.push(sql::literal(*kind, value));
        }
        batch.push(literals);
        returned += 1;

        if batch.len() >= context.options.batch_rows {
            write_batch(context, table, &names, &keep, std::mem::take(&mut batch)).await?;
        }
    }
    write_batch(context, table, &names, &keep, batch).await?;
    Ok(returned)
}

/// Which columns of a query's rows to remember: for the table, and for the sampler's own name.
struct Keep {
    table: Vec<(String, usize)>,
    named: Option<(String, Vec<(String, usize)>)>,
}

async fn write_batch(
    context: &Shared,
    table: &str,
    names: &[String],
    keep: &Keep,
    batch: Vec<Vec<Vec<u8>>>,
) -> Result<()> {
    // A named sampler's keys come from every row it returned, even rows another sampler of the
    // table already wrote.
    if let Some((name, columns)) = &keep.named {
        let mut state = context.named[name].lock().unwrap();
        for row in &batch {
            for (column, index) in columns {
                if row[*index] != sql::NULL {
                    state.keys.get_mut(column).unwrap().insert(&row[*index]);
                }
            }
        }
    }

    let rows = {
        let dedupe = context.plan.tables[table].dedupe;
        let mut state = context.tables[table].lock().unwrap();
        let mut rows = Vec::with_capacity(batch.len());
        for row in batch {
            if dedupe && !state.seen.insert(row_hash(&row)) {
                continue;
            }
            for (column, index) in &keep.table {
                if row[*index] != sql::NULL {
                    state.keys.get_mut(column).unwrap().insert(&row[*index]);
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

fn row_hash(row: &[Vec<u8>]) -> u128 {
    let mut buffer = Vec::new();
    for literal in row {
        buffer.extend_from_slice(&(literal.len() as u64).to_le_bytes());
        buffer.extend_from_slice(literal);
    }
    xxh3_128(&buffer)
}

fn log(context: &Shared, message: &str) {
    eprintln!(
        "[{:>7.2}s] {message}",
        context.started.elapsed().as_secs_f64()
    );
}
