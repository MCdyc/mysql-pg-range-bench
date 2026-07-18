use std::cmp::min;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Duration, NaiveDateTime, SecondsFormat, Timelike, Utc};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, MySql, Postgres, QueryBuilder};

const FIELD_COUNT: usize = 15;
// PostgreSQL and MySQL both cap prepared-statement parameters at 65,535. Keeping
// a little headroom also makes failures easier to diagnose if a field is added.
const MAX_BIND_PARAMETERS: usize = 60_000;
const DEFAULT_ROWS: u64 = 30_000_000;
const DEFAULT_SCAN_ROWS: u64 = 5_000_000;
const DEFAULT_SKIP_LOCKED_ROWS: u64 = 500;
const DEFAULT_SKIP_LOCKED_HELD_ROWS: u64 = 100;
const MAX_QUERY_EXECUTIONS: u32 = 100_000;
const GENERATOR_VERSION: &str = "splitmix64-v1";
const FINGERPRINT_ALGORITHM: &str = "fnv1a64-length-prefixed-v1";

const REGIONS: [&str; 8] = [
    "north",
    "south",
    "east",
    "west",
    "central",
    "northeast",
    "northwest",
    "coastal",
];
const DEVICES: [&str; 6] = ["android", "ios", "web", "tablet", "desktop", "other"];
const CITIES: [&str; 12] = [
    "beijing",
    "shanghai",
    "shenzhen",
    "guangzhou",
    "hangzhou",
    "chengdu",
    "wuhan",
    "nanjing",
    "xiamen",
    "suzhou",
    "tianjin",
    "qingdao",
];
const SOURCES: [&str; 6] = [
    "organic", "ads", "referral", "direct", "partner", "campaign",
];

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DatabaseSelection {
    Both,
    Mysql,
    #[value(alias = "pg")]
    Postgres,
}

impl DatabaseSelection {
    fn includes_mysql(self) -> bool {
        matches!(self, Self::Both | Self::Mysql)
    }

    fn includes_postgres(self) -> bool {
        matches!(self, Self::Both | Self::Postgres)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Mysql => "mysql",
            Self::Postgres => "postgres",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum QueryRangeMode {
    Same,
    Different,
}

impl QueryRangeMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Different => "different",
        }
    }
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Databases to benchmark. Use `pg` as an alias for `postgres`.
    #[arg(long, env = "BENCH_DATABASE", value_enum, default_value_t = DatabaseSelection::Both)]
    database: DatabaseSelection,

    /// MySQL connection URL. It is never written to the result file.
    #[arg(
        long,
        env = "MYSQL_URL",
        hide_env_values = true,
        hide_default_value = true,
        default_value = "mysql://benchmark:benchmark_password@127.0.0.1:3306/benchmark"
    )]
    mysql_url: String,

    /// PostgreSQL connection URL. It is never written to the result file.
    #[arg(
        long,
        env = "POSTGRES_URL",
        hide_env_values = true,
        hide_default_value = true,
        default_value = "postgres://benchmark:benchmark_password@127.0.0.1:5432/benchmark"
    )]
    postgres_url: String,

    /// Table name (portable unqualified identifier, at most 40 characters).
    #[arg(long, env = "BENCH_TABLE", default_value = "benchmark_events")]
    table: String,

    /// Number of deterministic rows inserted into each database.
    #[arg(long, env = "BENCH_ROWS", default_value_t = DEFAULT_ROWS)]
    rows: u64,

    /// Number of rows in the indexed range-count query.
    #[arg(long, env = "BENCH_SCAN_ROWS", default_value_t = DEFAULT_SCAN_ROWS)]
    scan_rows: u64,

    /// Rows in the indexed SKIP LOCKED candidate range.
    #[arg(
        long,
        env = "BENCH_SKIP_LOCKED_ROWS",
        default_value_t = DEFAULT_SKIP_LOCKED_ROWS
    )]
    skip_locked_rows: u64,

    /// Leading candidate rows held by another transaction during SKIP LOCKED.
    #[arg(
        long,
        env = "BENCH_SKIP_LOCKED_HELD_ROWS",
        default_value_t = DEFAULT_SKIP_LOCKED_HELD_ROWS
    )]
    skip_locked_held_rows: u64,

    /// Zero-based first generated row in the query range. Defaults to a centered range.
    #[arg(long, env = "BENCH_RANGE_START_ROW")]
    range_start_row: Option<u64>,

    /// Rows per multi-value INSERT. At 15 fields, the maximum accepted value is 4,000.
    #[arg(long, env = "BENCH_BATCH_SIZE", default_value_t = 1_000)]
    batch_size: usize,

    /// Maximum rows committed in one transaction.
    #[arg(long, env = "BENCH_TRANSACTION_ROWS", default_value_t = 100_000)]
    transaction_rows: u64,

    /// Warm-up range queries per database (recorded but excluded from the summary).
    #[arg(long, env = "BENCH_WARMUPS", default_value_t = 2)]
    warmups: u32,

    /// Measured range queries per database.
    #[arg(long, env = "BENCH_RUNS", default_value_t = 5)]
    runs: u32,

    /// Reuse one range or spread measured queries across different ranges.
    #[arg(
        long,
        env = "BENCH_QUERY_RANGES",
        value_enum,
        default_value_t = QueryRangeMode::Same
    )]
    query_ranges: QueryRangeMode,

    /// Maximum database connections in each pool.
    #[arg(long, env = "BENCH_POOL_SIZE", default_value_t = 1)]
    pool_size: u32,

    /// Data-generator seed. The same seed and row number produce the same row.
    #[arg(long, env = "BENCH_SEED", default_value_t = 20_260_715)]
    seed: u64,

    /// UTC RFC 3339 event_time for row zero. Every following row is exactly +1 second.
    #[arg(long, env = "BENCH_BASE_TIME", default_value = "2024-01-01T00:00:00Z")]
    base_time: String,

    /// Progress message interval during insertion. Set to 0 to disable.
    #[arg(long, env = "BENCH_PROGRESS_EVERY", default_value_t = 1_000_000)]
    progress_every: u64,

    /// Use an existing table; run maintenance, EXPLAIN, warmups, and timed queries.
    #[arg(long, env = "BENCH_SKIP_INSERT", default_value_t = false)]
    skip_insert: bool,

    /// Skip ANALYZE/VACUUM maintenance before the range query.
    #[arg(long, env = "BENCH_SKIP_MAINTENANCE", default_value_t = false)]
    skip_maintenance: bool,

    /// Skip the separate concurrent SKIP LOCKED validation.
    #[arg(long, env = "BENCH_SKIP_LOCK_TEST", default_value_t = false)]
    skip_lock_test: bool,

    /// Structured JSON result destination.
    #[arg(
        long,
        env = "BENCH_OUTPUT",
        default_value = "benchmark-results/run.json"
    )]
    output: PathBuf,
}

#[derive(Debug)]
struct ResolvedConfig {
    selection: DatabaseSelection,
    mysql_url: String,
    postgres_url: String,
    table: String,
    rows: u64,
    scan_rows: u64,
    skip_locked_rows: u64,
    skip_locked_held_rows: u64,
    range_start_row: u64,
    batch_size: usize,
    transaction_rows: u64,
    warmups: u32,
    runs: u32,
    query_ranges: QueryRangeMode,
    pool_size: u32,
    seed: u64,
    base_time: NaiveDateTime,
    lower_bound: NaiveDateTime,
    upper_bound: NaiveDateTime,
    skip_locked_upper_bound: NaiveDateTime,
    progress_every: u64,
    skip_insert: bool,
    skip_maintenance: bool,
    skip_lock_test: bool,
    output: PathBuf,
}

impl ResolvedConfig {
    fn from_args(args: Args) -> Result<Self> {
        validate_identifier(&args.table)?;
        ensure!(args.rows > 0, "--rows must be greater than zero");
        ensure!(
            args.rows <= i64::MAX as u64,
            "--rows is too large for BIGINT row identifiers"
        );
        ensure!(args.scan_rows > 0, "--scan-rows must be greater than zero");
        ensure!(
            args.scan_rows <= args.rows,
            "--scan-rows ({}) cannot exceed --rows ({})",
            args.scan_rows,
            args.rows
        );
        if !args.skip_lock_test {
            ensure!(
                args.skip_locked_rows > 1,
                "--skip-locked-rows must be greater than one"
            );
            ensure!(
                args.skip_locked_rows <= args.scan_rows,
                "--skip-locked-rows ({}) cannot exceed --scan-rows ({})",
                args.skip_locked_rows,
                args.scan_rows
            );
            ensure!(
                args.skip_locked_held_rows > 0
                    && args.skip_locked_held_rows < args.skip_locked_rows,
                "--skip-locked-held-rows must be between 1 and --skip-locked-rows - 1"
            );
        }
        ensure!(
            args.batch_size > 0,
            "--batch-size must be greater than zero"
        );
        let bind_parameters = args
            .batch_size
            .checked_mul(FIELD_COUNT)
            .context("--batch-size is too large")?;
        ensure!(
            bind_parameters <= MAX_BIND_PARAMETERS,
            "--batch-size {} uses {} bind parameters (15 per row); maximum is {} parameters, so batch size must be <= {}",
            args.batch_size,
            bind_parameters,
            MAX_BIND_PARAMETERS,
            MAX_BIND_PARAMETERS / FIELD_COUNT
        );
        ensure!(
            args.transaction_rows > 0,
            "--transaction-rows must be greater than zero"
        );
        ensure!(args.runs > 0, "--runs must be greater than zero");
        if matches!(args.query_ranges, QueryRangeMode::Different) {
            ensure!(
                args.warmups == 0,
                "--query-ranges different requires --warmups 0"
            );
            ensure!(
                args.runs > 1,
                "--query-ranges different requires --runs greater than one"
            );
            ensure!(
                args.range_start_row.is_none(),
                "--range-start-row cannot be combined with --query-ranges different"
            );
            ensure!(
                args.rows - args.scan_rows >= u64::from(args.runs - 1),
                "--query-ranges different cannot produce {} unique ranges from {} rows with --scan-rows {}",
                args.runs,
                args.rows,
                args.scan_rows
            );
        }
        let query_executions = args
            .warmups
            .checked_add(args.runs)
            .context("--warmups plus --runs is too large")?;
        ensure!(
            query_executions <= MAX_QUERY_EXECUTIONS,
            "--warmups plus --runs cannot exceed {MAX_QUERY_EXECUTIONS}"
        );
        ensure!(args.pool_size > 0, "--pool-size must be greater than zero");

        let range_start_row = match args.query_ranges {
            QueryRangeMode::Same => args
                .range_start_row
                .unwrap_or((args.rows - args.scan_rows) / 2),
            QueryRangeMode::Different => 0,
        };
        let range_end_row = range_start_row
            .checked_add(args.scan_rows)
            .context("query range row numbers overflowed u64")?;
        ensure!(
            range_end_row <= args.rows,
            "query range [{}..{}) lies outside generated rows [0..{})",
            range_start_row,
            range_end_row,
            args.rows
        );

        let base_time = DateTime::parse_from_rfc3339(&args.base_time)
            .with_context(|| {
                format!(
                    "invalid --base-time {:?}; expected RFC 3339 such as 2024-01-01T00:00:00Z",
                    args.base_time
                )
            })?
            .with_timezone(&Utc)
            .naive_utc();
        ensure!(
            base_time.nanosecond().is_multiple_of(1_000),
            "--base-time must be aligned to whole microseconds so MySQL and PostgreSQL store the identical value"
        );
        // Keep the shared dataset inside MySQL DATETIME's portable year range.
        // PostgreSQL accepts a wider range, but accepting it for only one target would
        // make a later two-database rerun fail with the same recorded configuration.
        let final_event_time = event_time_at(base_time, args.rows - 1)?;
        let lower_bound = event_time_at(base_time, range_start_row)?;
        let upper_bound = event_time_at(base_time, range_end_row)?;
        let skip_locked_upper_bound = if args.skip_lock_test {
            lower_bound
        } else {
            let skip_locked_end_row = range_start_row
                .checked_add(args.skip_locked_rows)
                .context("SKIP LOCKED range row numbers overflowed u64")?;
            event_time_at(base_time, skip_locked_end_row)?
        };
        ensure!(
            base_time.year() >= 1000
                && final_event_time.year() <= 9999
                && (1000..=9999).contains(&lower_bound.year())
                && upper_bound.year() <= 9999,
            "generated event_time values and the exclusive query bound must stay within MySQL DATETIME years 1000..=9999"
        );

        Ok(Self {
            selection: args.database,
            mysql_url: args.mysql_url,
            postgres_url: args.postgres_url,
            table: args.table,
            rows: args.rows,
            scan_rows: args.scan_rows,
            skip_locked_rows: args.skip_locked_rows,
            skip_locked_held_rows: args.skip_locked_held_rows,
            range_start_row,
            batch_size: args.batch_size,
            transaction_rows: args.transaction_rows,
            warmups: args.warmups,
            runs: args.runs,
            query_ranges: args.query_ranges,
            pool_size: args.pool_size,
            seed: args.seed,
            base_time,
            lower_bound,
            upper_bound,
            skip_locked_upper_bound,
            progress_every: args.progress_every,
            skip_insert: args.skip_insert,
            skip_maintenance: args.skip_maintenance,
            skip_lock_test: args.skip_lock_test,
            output: args.output,
        })
    }
}

#[derive(Debug)]
struct BenchRow {
    id: i64,
    event_time: NaiveDateTime,
    user_id: i64,
    order_id: i64,
    category_id: i32,
    status: i32,
    quantity: i32,
    score: i32,
    region: &'static str,
    device: &'static str,
    customer_name: String,
    email: String,
    city: &'static str,
    note: String,
    source: &'static str,
}

impl BenchRow {
    fn generate(index: u64, config: &ResolvedConfig) -> Result<Self> {
        let mut random = SplitMix64::for_row(config.seed, index);
        let r1 = random.next();
        let r2 = random.next();
        let r3 = random.next();
        let r4 = random.next();
        let r5 = random.next();
        let r6 = random.next();
        let r7 = random.next();
        let r8 = random.next();

        Ok(Self {
            id: i64::try_from(index + 1).context("row id does not fit BIGINT")?,
            event_time: event_time_at(config.base_time, index)?,
            user_id: (r1 % 5_000_000) as i64 + 1,
            order_id: (r2 % 900_000_000) as i64 + 1_000_000_000,
            category_id: (r3 % 1_000) as i32 + 1,
            status: (r4 % 8) as i32,
            quantity: (r5 % 20) as i32 + 1,
            score: (r6 % 10_001) as i32,
            region: REGIONS[(r1 % REGIONS.len() as u64) as usize],
            device: DEVICES[(r2 % DEVICES.len() as u64) as usize],
            customer_name: format!("user_{r7:016x}"),
            email: format!("u{r8:016x}@example.test"),
            city: CITIES[(r3 % CITIES.len() as u64) as usize],
            note: format!("note-{:016x}", r4 ^ r7),
            source: SOURCES[(r5 % SOURCES.len() as u64) as usize],
        })
    }
}

#[derive(Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn for_row(seed: u64, row: u64) -> Self {
        Self {
            // A row-specific stride avoids adjacent rows consuming overlapping parts
            // of the SplitMix64 stream while retaining random-access generation.
            state: seed ^ row.wrapping_mul(0xd134_2543_de82_ef95),
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

#[derive(Debug)]
struct Fingerprint {
    state: u64,
}

impl Fingerprint {
    fn new() -> Self {
        Self {
            state: 0xcbf2_9ce4_8422_2325,
        }
    }

    fn update_bytes(&mut self, bytes: &[u8]) {
        self.update_raw(&(bytes.len() as u64).to_le_bytes());
        self.update_raw(bytes);
    }

    fn update_raw(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn update_i64(&mut self, value: i64) {
        self.update_raw(&value.to_le_bytes());
    }

    fn update_i32(&mut self, value: i32) {
        self.update_raw(&value.to_le_bytes());
    }

    fn update_row(&mut self, row: &BenchRow) {
        self.update_i64(row.id);
        self.update_i64(row.event_time.and_utc().timestamp_micros());
        self.update_i64(row.user_id);
        self.update_i64(row.order_id);
        self.update_i32(row.category_id);
        self.update_i32(row.status);
        self.update_i32(row.quantity);
        self.update_i32(row.score);
        self.update_bytes(row.region.as_bytes());
        self.update_bytes(row.device.as_bytes());
        self.update_bytes(row.customer_name.as_bytes());
        self.update_bytes(row.email.as_bytes());
        self.update_bytes(row.city.as_bytes());
        self.update_bytes(row.note.as_bytes());
        self.update_bytes(row.source.as_bytes());
    }

    fn finish_hex(&self) -> String {
        format!("{:016x}", self.state)
    }
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    report_version: u32,
    started_at_utc: String,
    finished_at_utc: String,
    config: ReportConfig,
    data_model: DataModelReport,
    results: Vec<DatabaseReport>,
}

#[derive(Debug, Serialize)]
struct ReportConfig {
    database: String,
    table: String,
    rows: u64,
    scan_rows: u64,
    skip_locked_rows: u64,
    skip_locked_held_rows: u64,
    range_start_row: u64,
    batch_size: usize,
    transaction_rows: u64,
    warmups: u32,
    measured_runs: u32,
    query_ranges: &'static str,
    pool_size: u32,
    progress_every: u64,
    seed: u64,
    base_time_utc: String,
    skip_insert: bool,
    skip_maintenance: bool,
    skip_lock_test: bool,
}

#[derive(Debug, Serialize)]
struct DataModelReport {
    generator: &'static str,
    event_time_rule: &'static str,
    fingerprint_algorithm: &'static str,
    schema_status: &'static str,
    sample_rows_source: &'static str,
    primary_key_column: &'static str,
    indexed_column: &'static str,
    index_created_before_insert: Option<bool>,
    explicit_indexes: Option<u32>,
    columns: Vec<ColumnReport>,
    sample_rows: Vec<SampleRowReport>,
}

#[derive(Debug, Serialize)]
struct ColumnReport {
    name: &'static str,
    logical_type: &'static str,
    nullable: bool,
    value_rule: &'static str,
}

#[derive(Debug, Serialize)]
struct SampleRowReport {
    row_index_zero_based: u64,
    id: i64,
    event_time_utc: String,
    user_id: i64,
    order_id: i64,
    category_id: i32,
    status: i32,
    quantity: i32,
    score: i32,
    region: &'static str,
    device: &'static str,
    customer_name: String,
    email: String,
    city: &'static str,
    note: String,
    source: &'static str,
}

impl SampleRowReport {
    fn generate(row_index_zero_based: u64, config: &ResolvedConfig) -> Result<Self> {
        let row = BenchRow::generate(row_index_zero_based, config)?;
        Ok(Self {
            row_index_zero_based,
            id: row.id,
            event_time_utc: format_utc(row.event_time),
            user_id: row.user_id,
            order_id: row.order_id,
            category_id: row.category_id,
            status: row.status,
            quantity: row.quantity,
            score: row.score,
            region: row.region,
            device: row.device,
            customer_name: row.customer_name,
            email: row.email,
            city: row.city,
            note: row.note,
            source: row.source,
        })
    }
}

#[derive(Debug, Serialize)]
struct DatabaseReport {
    database: &'static str,
    server_version: String,
    schema_setup_ms: Option<f64>,
    insert: Option<InsertReport>,
    analyze_ms: Option<f64>,
    query: QueryReport,
    skip_locked: Option<SkipLockedReport>,
}

#[derive(Debug, Serialize)]
struct InsertReport {
    timing_scope: &'static str,
    includes_row_generation: bool,
    includes_fingerprint_calculation: bool,
    includes_progress_logging: bool,
    rows: u64,
    batches: u64,
    transactions: u64,
    elapsed_ms: f64,
    rows_per_second: f64,
    generated_fingerprint: String,
}

#[derive(Debug, Serialize)]
struct QueryReport {
    sql: String,
    connection_scope: &'static str,
    explain: ExplainReport,
    explain_range: &'static str,
    range_mode: &'static str,
    range_semantics: &'static str,
    lower_bound_utc: String,
    upper_bound_utc: String,
    expected_count: u64,
    observed_count: u64,
    warmup_ms: Vec<f64>,
    measured_ms: Vec<f64>,
    measured_queries: Vec<TimedRangeQueryReport>,
    summary_ms: TimingSummary,
}

#[derive(Debug, Serialize)]
struct TimedRangeQueryReport {
    run_number: u32,
    range_start_row: u64,
    range_end_row: u64,
    lower_bound_utc: String,
    upper_bound_utc: String,
    expected_count: u64,
    observed_count: u64,
    elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct SkipLockedReport {
    holder_sql: String,
    worker_sql: String,
    connection_scope: &'static str,
    transaction_isolation: &'static str,
    range_semantics: &'static str,
    lower_bound_utc: String,
    upper_bound_utc: String,
    candidate_rows_expected: u64,
    candidate_rows_observed: u64,
    held_rows_expected: u64,
    held_rows_observed: u64,
    returned_rows_expected: u64,
    returned_rows_observed: u64,
    elapsed_ms: f64,
    expected_index: String,
    explain_uses_expected_index: bool,
    explain: ExplainReport,
    transactions_rolled_back: bool,
}

#[derive(Debug, Serialize)]
struct ExplainReport {
    format: &'static str,
    analyze: bool,
    plan: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct TimingSummary {
    min: f64,
    max: f64,
    mean: f64,
    median: f64,
    p95: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    if Path::new(".env").exists() {
        dotenvy::dotenv().context("load .env from the current directory")?;
    }
    let config = ResolvedConfig::from_args(Args::parse())?;
    let started_at = Utc::now();

    eprintln!(
        "benchmark: rows={}, scan_rows={}, range=[{}..{}), query_ranges={}, warmups={}, measured_runs={}, skip_locked_rows={}, skip_locked_held_rows={}, batch_size={}, transaction_rows={}, maintenance={}, lock_test={}",
        config.rows,
        config.scan_rows,
        config.range_start_row,
        config.range_start_row + config.scan_rows,
        config.query_ranges.as_str(),
        config.warmups,
        config.runs,
        config.skip_locked_rows,
        config.skip_locked_held_rows,
        config.batch_size,
        config.transaction_rows,
        !config.skip_maintenance,
        !config.skip_lock_test
    );

    // Establish every requested connection before starting a potentially long insert.
    let mysql_pool = if config.selection.includes_mysql() {
        eprintln!("connecting to MySQL...");
        let pool = MySqlPoolOptions::new()
            .max_connections(config.pool_size)
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    sqlx::query("SET time_zone = '+00:00'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&config.mysql_url)
            .await
            .context("connect to MySQL")?;
        sqlx::query("SELECT 1")
            .execute(&pool)
            .await
            .context("MySQL preflight query")?;
        Some(pool)
    } else {
        None
    };
    let postgres_pool = if config.selection.includes_postgres() {
        eprintln!("connecting to PostgreSQL...");
        let pool = PgPoolOptions::new()
            .max_connections(config.pool_size)
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    sqlx::query("SET TIME ZONE 'UTC'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET max_parallel_workers_per_gather = 0")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&config.postgres_url)
            .await
            .context("connect to PostgreSQL")?;
        sqlx::query("SELECT 1")
            .execute(&pool)
            .await
            .context("PostgreSQL preflight query")?;
        Some(pool)
    } else {
        None
    };

    let mut results = Vec::with_capacity(2);
    if let Some(pool) = mysql_pool.as_ref() {
        results.push(run_mysql(pool, &config).await?);
    }
    if let Some(pool) = postgres_pool.as_ref() {
        results.push(run_postgres(pool, &config).await?);
    }

    for result in &results {
        print_console_summary(result);
    }

    if results.len() == 2 && !config.skip_insert {
        let mysql_fingerprint = results[0]
            .insert
            .as_ref()
            .context("missing first insert report")?
            .generated_fingerprint
            .as_str();
        let postgres_fingerprint = results[1]
            .insert
            .as_ref()
            .context("missing second insert report")?
            .generated_fingerprint
            .as_str();
        ensure!(
            mysql_fingerprint == postgres_fingerprint,
            "internal error: generated datasets differ between databases"
        );
    }

    let report = BenchmarkReport {
        report_version: 5,
        started_at_utc: format_utc(started_at.naive_utc()),
        finished_at_utc: format_utc(Utc::now().naive_utc()),
        config: report_config(&config),
        data_model: data_model_report(&config)?,
        results,
    };
    write_report(&config.output, &report)?;
    println!("result written to {}", config.output.display());

    Ok(())
}

async fn run_mysql(pool: &MySqlPool, config: &ResolvedConfig) -> Result<DatabaseReport> {
    eprintln!("MySQL: starting benchmark");
    let version: String = sqlx::query_scalar("SELECT VERSION()")
        .fetch_one(pool)
        .await
        .context("read MySQL version")?;

    let (schema_setup_ms, insert) = if config.skip_insert {
        (None, None)
    } else {
        let schema_started = Instant::now();
        recreate_mysql_schema(pool, &config.table).await?;
        let schema_ms = elapsed_ms(schema_started.elapsed());
        let inserted = insert_mysql(pool, config).await?;
        (Some(schema_ms), Some(inserted))
    };

    let analyze_ms = if config.skip_maintenance {
        None
    } else {
        let analyze_started = Instant::now();
        let analyze_sql = format!("ANALYZE TABLE {}", mysql_identifier(&config.table));
        sqlx::query(&analyze_sql)
            .fetch_all(pool)
            .await
            .context("ANALYZE MySQL table")?;
        Some(elapsed_ms(analyze_started.elapsed()))
    };

    let query = benchmark_mysql_query(pool, config).await?;
    let skip_locked = if config.skip_lock_test {
        None
    } else {
        Some(benchmark_mysql_skip_locked(config).await?)
    };
    Ok(DatabaseReport {
        database: "mysql",
        server_version: version,
        schema_setup_ms,
        insert,
        analyze_ms,
        query,
        skip_locked,
    })
}

async fn run_postgres(pool: &PgPool, config: &ResolvedConfig) -> Result<DatabaseReport> {
    eprintln!("PostgreSQL: starting benchmark");
    let version: String = sqlx::query_scalar("SELECT VERSION()")
        .fetch_one(pool)
        .await
        .context("read PostgreSQL version")?;

    let (schema_setup_ms, insert) = if config.skip_insert {
        (None, None)
    } else {
        let schema_started = Instant::now();
        recreate_postgres_schema(pool, &config.table).await?;
        let schema_ms = elapsed_ms(schema_started.elapsed());
        let inserted = insert_postgres(pool, config).await?;
        (Some(schema_ms), Some(inserted))
    };

    let analyze_ms = if config.skip_maintenance {
        None
    } else {
        let analyze_started = Instant::now();
        // VACUUM sets visibility-map bits, allowing a fair index-only COUNT(*) after bulk load.
        let vacuum_sql = format!("VACUUM (ANALYZE) {}", postgres_identifier(&config.table));
        pool.execute(vacuum_sql.as_str())
            .await
            .context("VACUUM ANALYZE PostgreSQL table")?;
        Some(elapsed_ms(analyze_started.elapsed()))
    };

    let query = benchmark_postgres_query(pool, config).await?;
    let skip_locked = if config.skip_lock_test {
        None
    } else {
        Some(benchmark_postgres_skip_locked(config).await?)
    };
    Ok(DatabaseReport {
        database: "postgres",
        server_version: version,
        schema_setup_ms,
        insert,
        analyze_ms,
        query,
        skip_locked,
    })
}

async fn recreate_mysql_schema(pool: &MySqlPool, table: &str) -> Result<()> {
    let index = mysql_identifier(&format!("idx_{table}_event_time"));
    let table = mysql_identifier(table);
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .context("drop old MySQL benchmark table")?;
    let create_table = format!(
        "CREATE TABLE {table} (\
         id BIGINT NOT NULL PRIMARY KEY,\
         event_time DATETIME(6) NOT NULL,\
         user_id BIGINT NOT NULL,\
         order_id BIGINT NOT NULL,\
         category_id INT NOT NULL,\
         status INT NOT NULL,\
         quantity INT NOT NULL,\
         score INT NOT NULL,\
         region VARCHAR(16) NOT NULL,\
         device VARCHAR(16) NOT NULL,\
         customer_name VARCHAR(32) NOT NULL,\
         email VARCHAR(64) NOT NULL,\
         city VARCHAR(32) NOT NULL,\
         note VARCHAR(64) NOT NULL,\
         source VARCHAR(16) NOT NULL\
         ) ENGINE=InnoDB"
    );
    pool.execute(create_table.as_str())
        .await
        .context("create MySQL benchmark table")?;
    let create_index = format!("CREATE INDEX {index} ON {table} (event_time)");
    pool.execute(create_index.as_str())
        .await
        .context("create MySQL event_time index before insertion")?;
    Ok(())
}

async fn recreate_postgres_schema(pool: &PgPool, table: &str) -> Result<()> {
    let index = postgres_identifier(&format!("idx_{table}_event_time"));
    let table = postgres_identifier(table);
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .context("drop old PostgreSQL benchmark table")?;
    let create_table = format!(
        "CREATE TABLE {table} (\
         id BIGINT NOT NULL PRIMARY KEY,\
         event_time TIMESTAMP(6) WITHOUT TIME ZONE NOT NULL,\
         user_id BIGINT NOT NULL,\
         order_id BIGINT NOT NULL,\
         category_id INTEGER NOT NULL,\
         status INTEGER NOT NULL,\
         quantity INTEGER NOT NULL,\
         score INTEGER NOT NULL,\
         region VARCHAR(16) NOT NULL,\
         device VARCHAR(16) NOT NULL,\
         customer_name VARCHAR(32) NOT NULL,\
         email VARCHAR(64) NOT NULL,\
         city VARCHAR(32) NOT NULL,\
         note VARCHAR(64) NOT NULL,\
         source VARCHAR(16) NOT NULL\
         )"
    );
    pool.execute(create_table.as_str())
        .await
        .context("create PostgreSQL benchmark table")?;
    let create_index = format!("CREATE INDEX {index} ON {table} (event_time)");
    pool.execute(create_index.as_str())
        .await
        .context("create PostgreSQL event_time index before insertion")?;
    Ok(())
}

async fn insert_mysql(pool: &MySqlPool, config: &ResolvedConfig) -> Result<InsertReport> {
    let started = Instant::now();
    let mut offset = 0_u64;
    let mut batches = 0_u64;
    let mut transactions = 0_u64;
    let mut next_progress = config.progress_every;
    let mut fingerprint = Fingerprint::new();
    let table = mysql_identifier(&config.table);

    while offset < config.rows {
        let transaction_end = min(offset.saturating_add(config.transaction_rows), config.rows);
        let mut transaction = pool.begin().await.context("begin MySQL transaction")?;

        while offset < transaction_end {
            let batch_end = min(
                offset.saturating_add(config.batch_size as u64),
                transaction_end,
            );
            let rows = generate_batch(offset, batch_end, config)?;
            for row in &rows {
                fingerprint.update_row(row);
            }

            let mut builder = QueryBuilder::<MySql>::new(format!(
                "INSERT INTO {table} (id, event_time, user_id, order_id, category_id, status, quantity, score, region, device, customer_name, email, city, note, source) "
            ));
            builder.push_values(rows.iter(), |mut values, row| {
                values
                    .push_bind(row.id)
                    .push_bind(row.event_time)
                    .push_bind(row.user_id)
                    .push_bind(row.order_id)
                    .push_bind(row.category_id)
                    .push_bind(row.status)
                    .push_bind(row.quantity)
                    .push_bind(row.score)
                    .push_bind(row.region)
                    .push_bind(row.device)
                    .push_bind(&row.customer_name)
                    .push_bind(&row.email)
                    .push_bind(row.city)
                    .push_bind(&row.note)
                    .push_bind(row.source);
            });
            let result = builder
                .build()
                .execute(&mut *transaction)
                .await
                .with_context(|| format!("insert MySQL rows [{offset}..{batch_end})"))?;
            ensure!(
                result.rows_affected() == batch_end - offset,
                "MySQL reported {} affected rows for batch [{offset}..{batch_end})",
                result.rows_affected()
            );
            offset = batch_end;
            batches += 1;
        }

        transaction
            .commit()
            .await
            .context("commit MySQL transaction")?;
        transactions += 1;
        report_progress(
            "MySQL",
            offset,
            config.rows,
            started.elapsed(),
            config.progress_every,
            &mut next_progress,
        );
    }

    Ok(insert_report(
        config.rows,
        batches,
        transactions,
        started.elapsed(),
        fingerprint.finish_hex(),
    ))
}

async fn insert_postgres(pool: &PgPool, config: &ResolvedConfig) -> Result<InsertReport> {
    let started = Instant::now();
    let mut offset = 0_u64;
    let mut batches = 0_u64;
    let mut transactions = 0_u64;
    let mut next_progress = config.progress_every;
    let mut fingerprint = Fingerprint::new();
    let table = postgres_identifier(&config.table);

    while offset < config.rows {
        let transaction_end = min(offset.saturating_add(config.transaction_rows), config.rows);
        let mut transaction = pool.begin().await.context("begin PostgreSQL transaction")?;

        while offset < transaction_end {
            let batch_end = min(
                offset.saturating_add(config.batch_size as u64),
                transaction_end,
            );
            let rows = generate_batch(offset, batch_end, config)?;
            for row in &rows {
                fingerprint.update_row(row);
            }

            let mut builder = QueryBuilder::<Postgres>::new(format!(
                "INSERT INTO {table} (id, event_time, user_id, order_id, category_id, status, quantity, score, region, device, customer_name, email, city, note, source) "
            ));
            builder.push_values(rows.iter(), |mut values, row| {
                values
                    .push_bind(row.id)
                    .push_bind(row.event_time)
                    .push_bind(row.user_id)
                    .push_bind(row.order_id)
                    .push_bind(row.category_id)
                    .push_bind(row.status)
                    .push_bind(row.quantity)
                    .push_bind(row.score)
                    .push_bind(row.region)
                    .push_bind(row.device)
                    .push_bind(&row.customer_name)
                    .push_bind(&row.email)
                    .push_bind(row.city)
                    .push_bind(&row.note)
                    .push_bind(row.source);
            });
            let result = builder
                .build()
                .execute(&mut *transaction)
                .await
                .with_context(|| format!("insert PostgreSQL rows [{offset}..{batch_end})"))?;
            ensure!(
                result.rows_affected() == batch_end - offset,
                "PostgreSQL reported {} affected rows for batch [{offset}..{batch_end})",
                result.rows_affected()
            );
            offset = batch_end;
            batches += 1;
        }

        transaction
            .commit()
            .await
            .context("commit PostgreSQL transaction")?;
        transactions += 1;
        report_progress(
            "PostgreSQL",
            offset,
            config.rows,
            started.elapsed(),
            config.progress_every,
            &mut next_progress,
        );
    }

    Ok(insert_report(
        config.rows,
        batches,
        transactions,
        started.elapsed(),
        fingerprint.finish_hex(),
    ))
}

fn generate_batch(start: u64, end: u64, config: &ResolvedConfig) -> Result<Vec<BenchRow>> {
    let capacity = usize::try_from(end - start).context("batch length does not fit usize")?;
    let mut rows = Vec::with_capacity(capacity);
    for index in start..end {
        rows.push(BenchRow::generate(index, config)?);
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy)]
struct QueryRange {
    start_row: u64,
    end_row: u64,
    lower_bound: NaiveDateTime,
    upper_bound: NaiveDateTime,
}

fn measured_query_range(config: &ResolvedConfig, measured_index: u32) -> Result<QueryRange> {
    ensure!(
        measured_index < config.runs,
        "measured query index {measured_index} is outside configured runs {}",
        config.runs
    );
    let start_row = match config.query_ranges {
        QueryRangeMode::Same => config.range_start_row,
        QueryRangeMode::Different => {
            let numerator = u128::from(measured_index) * u128::from(config.rows - config.scan_rows);
            let denominator = u128::from(config.runs - 1);
            u64::try_from(numerator / denominator)
                .context("different query range start does not fit u64")?
        }
    };
    let end_row = start_row
        .checked_add(config.scan_rows)
        .context("measured query range row numbers overflowed u64")?;
    ensure!(
        end_row <= config.rows,
        "measured query range [{start_row}..{end_row}) lies outside rows [0..{})",
        config.rows
    );
    Ok(QueryRange {
        start_row,
        end_row,
        lower_bound: event_time_at(config.base_time, start_row)?,
        upper_bound: event_time_at(config.base_time, end_row)?,
    })
}

fn timed_query_report(
    run_number: u32,
    range: QueryRange,
    expected_count: u64,
    observed_count: u64,
    elapsed_ms: f64,
) -> TimedRangeQueryReport {
    TimedRangeQueryReport {
        run_number,
        range_start_row: range.start_row,
        range_end_row: range.end_row,
        lower_bound_utc: format_utc(range.lower_bound),
        upper_bound_utc: format_utc(range.upper_bound),
        expected_count,
        observed_count,
        elapsed_ms,
    }
}

fn print_measured_query(database: &str, total_runs: u32, query: &TimedRangeQueryReport) {
    eprintln!(
        "{database} measured query {}/{}: rows=[{}..{}), time=[{}..{}), COUNT(*)={}, elapsed_ms={:.3}",
        query.run_number,
        total_runs,
        query.range_start_row,
        query.range_end_row,
        query.lower_bound_utc,
        query.upper_bound_utc,
        query.observed_count,
        query.elapsed_ms
    );
}

async fn benchmark_mysql_query(pool: &MySqlPool, config: &ResolvedConfig) -> Result<QueryReport> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE event_time >= ? AND event_time < ?",
        mysql_identifier(&config.table)
    );
    let mut connection = pool
        .acquire()
        .await
        .context("acquire one MySQL query connection")?;
    let explain_range = measured_query_range(config, 0)?;
    let explain_sql = format!("EXPLAIN FORMAT=JSON {sql}");
    let explain_text: String = sqlx::query_scalar(&explain_sql)
        .bind(explain_range.lower_bound)
        .bind(explain_range.upper_bound)
        .fetch_one(&mut *connection)
        .await
        .context("EXPLAIN MySQL range COUNT(*)")?;
    let explain_plan = serde_json::from_str(&explain_text).context(
        "parse MySQL EXPLAIN FORMAT=JSON output; ensure end_markers_in_json is disabled",
    )?;
    let explain = ExplainReport {
        format: "json",
        analyze: false,
        plan: explain_plan,
    };
    let mut warmup_ms = Vec::with_capacity(config.warmups as usize);
    let mut measured_queries = Vec::with_capacity(config.runs as usize);
    let mut observed_count = 0_u64;

    for _ in 0..config.warmups {
        let started = Instant::now();
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(config.lower_bound)
            .bind(config.upper_bound)
            .fetch_one(&mut *connection)
            .await
            .context("execute MySQL range COUNT(*)")?;
        let duration = elapsed_ms(started.elapsed());
        observed_count = validate_count("MySQL", count, config.scan_rows)?;
        warmup_ms.push(duration);
    }

    for measured_index in 0..config.runs {
        let range = measured_query_range(config, measured_index)?;
        let started = Instant::now();
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(range.lower_bound)
            .bind(range.upper_bound)
            .fetch_one(&mut *connection)
            .await
            .with_context(|| {
                format!(
                    "execute MySQL measured range COUNT(*) run {}",
                    measured_index + 1
                )
            })?;
        let duration = elapsed_ms(started.elapsed());
        observed_count = validate_count("MySQL", count, config.scan_rows)?;
        let measured = timed_query_report(
            measured_index + 1,
            range,
            config.scan_rows,
            observed_count,
            duration,
        );
        print_measured_query("MySQL", config.runs, &measured);
        measured_queries.push(measured);
    }

    Ok(query_report(
        sql,
        explain,
        explain_range,
        config,
        observed_count,
        warmup_ms,
        measured_queries,
    ))
}

async fn benchmark_postgres_query(pool: &PgPool, config: &ResolvedConfig) -> Result<QueryReport> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE event_time >= $1 AND event_time < $2",
        postgres_identifier(&config.table)
    );
    let mut connection = pool
        .acquire()
        .await
        .context("acquire one PostgreSQL query connection")?;
    let explain_range = measured_query_range(config, 0)?;
    let explain_sql = format!("EXPLAIN (FORMAT JSON) {sql}");
    let explain_plan: serde_json::Value = sqlx::query_scalar(&explain_sql)
        .bind(explain_range.lower_bound)
        .bind(explain_range.upper_bound)
        .fetch_one(&mut *connection)
        .await
        .context("EXPLAIN PostgreSQL range COUNT(*)")?;
    let explain = ExplainReport {
        format: "json",
        analyze: false,
        plan: explain_plan,
    };
    let mut warmup_ms = Vec::with_capacity(config.warmups as usize);
    let mut measured_queries = Vec::with_capacity(config.runs as usize);
    let mut observed_count = 0_u64;

    for _ in 0..config.warmups {
        let started = Instant::now();
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(config.lower_bound)
            .bind(config.upper_bound)
            .fetch_one(&mut *connection)
            .await
            .context("execute PostgreSQL range COUNT(*)")?;
        let duration = elapsed_ms(started.elapsed());
        observed_count = validate_count("PostgreSQL", count, config.scan_rows)?;
        warmup_ms.push(duration);
    }

    for measured_index in 0..config.runs {
        let range = measured_query_range(config, measured_index)?;
        let started = Instant::now();
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(range.lower_bound)
            .bind(range.upper_bound)
            .fetch_one(&mut *connection)
            .await
            .with_context(|| {
                format!(
                    "execute PostgreSQL measured range COUNT(*) run {}",
                    measured_index + 1
                )
            })?;
        let duration = elapsed_ms(started.elapsed());
        observed_count = validate_count("PostgreSQL", count, config.scan_rows)?;
        let measured = timed_query_report(
            measured_index + 1,
            range,
            config.scan_rows,
            observed_count,
            duration,
        );
        print_measured_query("PostgreSQL", config.runs, &measured);
        measured_queries.push(measured);
    }

    Ok(query_report(
        sql,
        explain,
        explain_range,
        config,
        observed_count,
        warmup_ms,
        measured_queries,
    ))
}

async fn benchmark_mysql_skip_locked(config: &ResolvedConfig) -> Result<SkipLockedReport> {
    let table = mysql_identifier(&config.table);
    let expected_index = format!("idx_{}_event_time", config.table);
    let holder_sql = format!(
        "SELECT id FROM {table} WHERE event_time >= ? AND event_time < ? ORDER BY event_time LIMIT {} FOR UPDATE",
        config.skip_locked_held_rows
    );
    let worker_sql = format!(
        "SELECT id FROM {table} WHERE event_time >= ? AND event_time < ? ORDER BY event_time FOR UPDATE SKIP LOCKED"
    );
    let count_sql =
        format!("SELECT COUNT(*) FROM {table} WHERE event_time >= ? AND event_time < ?");

    let pool = MySqlPoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET time_zone = '+00:00'")
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&config.mysql_url)
        .await
        .context("connect two MySQL SKIP LOCKED sessions")?;

    let candidate_count: i64 = sqlx::query_scalar(&count_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_one(&pool)
        .await
        .context("count MySQL SKIP LOCKED candidate rows")?;
    let candidate_rows_observed = validate_count(
        "MySQL SKIP LOCKED candidate",
        candidate_count,
        config.skip_locked_rows,
    )?;

    let explain_sql = format!("EXPLAIN FORMAT=JSON {worker_sql}");
    let explain_text: String = sqlx::query_scalar(&explain_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_one(&pool)
        .await
        .context("EXPLAIN MySQL SKIP LOCKED query")?;
    let explain_plan: serde_json::Value =
        serde_json::from_str(&explain_text).context("parse MySQL SKIP LOCKED EXPLAIN JSON")?;
    let explain_uses_expected_index = json_field_equals(&explain_plan, "key", &expected_index);
    ensure!(
        explain_uses_expected_index,
        "MySQL SKIP LOCKED EXPLAIN did not select expected event_time index {expected_index}"
    );
    let explain = ExplainReport {
        format: "json",
        analyze: false,
        plan: explain_plan,
    };

    let mut holder_transaction = pool
        .begin()
        .await
        .context("begin MySQL lock-holder transaction")?;
    let held_ids: Vec<i64> = sqlx::query_scalar(&holder_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_all(&mut *holder_transaction)
        .await
        .context("lock the leading MySQL SKIP LOCKED rows")?;
    let held_rows_observed =
        u64::try_from(held_ids.len()).context("MySQL held-row count does not fit u64")?;
    ensure!(
        held_rows_observed == config.skip_locked_held_rows,
        "MySQL lock holder selected {held_rows_observed} rows, expected exactly {}",
        config.skip_locked_held_rows
    );

    let mut worker_transaction = pool
        .begin()
        .await
        .context("begin MySQL SKIP LOCKED worker transaction")?;
    let started = Instant::now();
    let returned_result = sqlx::query_scalar(&worker_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_all(&mut *worker_transaction)
        .await;
    let duration = elapsed_ms(started.elapsed());

    let worker_rollback = worker_transaction.rollback().await;
    let holder_rollback = holder_transaction.rollback().await;
    let returned_ids: Vec<i64> = returned_result.context("execute MySQL FOR UPDATE SKIP LOCKED")?;
    worker_rollback.context("roll back MySQL SKIP LOCKED worker transaction")?;
    holder_rollback.context("roll back MySQL lock-holder transaction")?;
    pool.close().await;

    let returned_rows_expected = config.skip_locked_rows - config.skip_locked_held_rows;
    let returned_rows_observed = validate_skip_locked_ids(
        "MySQL",
        &held_ids,
        &returned_ids,
        config.skip_locked_held_rows,
        returned_rows_expected,
    )?;

    Ok(SkipLockedReport {
        holder_sql,
        worker_sql,
        connection_scope: "two dedicated concurrent connections; holder locks first, worker uses SKIP LOCKED",
        transaction_isolation: "READ COMMITTED",
        range_semantics: "first skip_locked_rows rows of the main indexed range",
        lower_bound_utc: format_utc(config.lower_bound),
        upper_bound_utc: format_utc(config.skip_locked_upper_bound),
        candidate_rows_expected: config.skip_locked_rows,
        candidate_rows_observed,
        held_rows_expected: config.skip_locked_held_rows,
        held_rows_observed,
        returned_rows_expected,
        returned_rows_observed,
        elapsed_ms: duration,
        expected_index,
        explain_uses_expected_index,
        explain,
        transactions_rolled_back: true,
    })
}

async fn benchmark_postgres_skip_locked(config: &ResolvedConfig) -> Result<SkipLockedReport> {
    let table = postgres_identifier(&config.table);
    let expected_index = format!("idx_{}_event_time", config.table);
    let holder_sql = format!(
        "SELECT id FROM {table} WHERE event_time >= $1 AND event_time < $2 ORDER BY event_time LIMIT {} FOR UPDATE",
        config.skip_locked_held_rows
    );
    let worker_sql = format!(
        "SELECT id FROM {table} WHERE event_time >= $1 AND event_time < $2 ORDER BY event_time FOR UPDATE SKIP LOCKED"
    );
    let count_sql =
        format!("SELECT COUNT(*) FROM {table} WHERE event_time >= $1 AND event_time < $2");

    let pool = PgPoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET TIME ZONE 'UTC'")
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET default_transaction_isolation = 'read committed'")
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET max_parallel_workers_per_gather = 0")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&config.postgres_url)
        .await
        .context("connect two PostgreSQL SKIP LOCKED sessions")?;

    let candidate_count: i64 = sqlx::query_scalar(&count_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_one(&pool)
        .await
        .context("count PostgreSQL SKIP LOCKED candidate rows")?;
    let candidate_rows_observed = validate_count(
        "PostgreSQL SKIP LOCKED candidate",
        candidate_count,
        config.skip_locked_rows,
    )?;

    let explain_sql = format!("EXPLAIN (FORMAT JSON) {worker_sql}");
    let explain_plan: serde_json::Value = sqlx::query_scalar(&explain_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_one(&pool)
        .await
        .context("EXPLAIN PostgreSQL SKIP LOCKED query")?;
    let explain_uses_expected_index =
        json_field_equals(&explain_plan, "Index Name", &expected_index);
    ensure!(
        explain_uses_expected_index,
        "PostgreSQL SKIP LOCKED EXPLAIN did not select expected event_time index {expected_index}"
    );
    let explain = ExplainReport {
        format: "json",
        analyze: false,
        plan: explain_plan,
    };

    let mut holder_transaction = pool
        .begin()
        .await
        .context("begin PostgreSQL lock-holder transaction")?;
    let held_ids: Vec<i64> = sqlx::query_scalar(&holder_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_all(&mut *holder_transaction)
        .await
        .context("lock the leading PostgreSQL SKIP LOCKED rows")?;
    let held_rows_observed =
        u64::try_from(held_ids.len()).context("PostgreSQL held-row count does not fit u64")?;
    ensure!(
        held_rows_observed == config.skip_locked_held_rows,
        "PostgreSQL lock holder selected {held_rows_observed} rows, expected exactly {}",
        config.skip_locked_held_rows
    );

    let mut worker_transaction = pool
        .begin()
        .await
        .context("begin PostgreSQL SKIP LOCKED worker transaction")?;
    let started = Instant::now();
    let returned_result = sqlx::query_scalar(&worker_sql)
        .bind(config.lower_bound)
        .bind(config.skip_locked_upper_bound)
        .fetch_all(&mut *worker_transaction)
        .await;
    let duration = elapsed_ms(started.elapsed());

    let worker_rollback = worker_transaction.rollback().await;
    let holder_rollback = holder_transaction.rollback().await;
    let returned_ids: Vec<i64> =
        returned_result.context("execute PostgreSQL FOR UPDATE SKIP LOCKED")?;
    worker_rollback.context("roll back PostgreSQL SKIP LOCKED worker transaction")?;
    holder_rollback.context("roll back PostgreSQL lock-holder transaction")?;
    pool.close().await;

    let returned_rows_expected = config.skip_locked_rows - config.skip_locked_held_rows;
    let returned_rows_observed = validate_skip_locked_ids(
        "PostgreSQL",
        &held_ids,
        &returned_ids,
        config.skip_locked_held_rows,
        returned_rows_expected,
    )?;

    Ok(SkipLockedReport {
        holder_sql,
        worker_sql,
        connection_scope: "two dedicated concurrent connections; holder locks first, worker uses SKIP LOCKED",
        transaction_isolation: "READ COMMITTED",
        range_semantics: "first skip_locked_rows rows of the main indexed range",
        lower_bound_utc: format_utc(config.lower_bound),
        upper_bound_utc: format_utc(config.skip_locked_upper_bound),
        candidate_rows_expected: config.skip_locked_rows,
        candidate_rows_observed,
        held_rows_expected: config.skip_locked_held_rows,
        held_rows_observed,
        returned_rows_expected,
        returned_rows_observed,
        elapsed_ms: duration,
        expected_index,
        explain_uses_expected_index,
        explain,
        transactions_rolled_back: true,
    })
}

fn validate_skip_locked_ids(
    database: &str,
    held_ids: &[i64],
    returned_ids: &[i64],
    expected_held: u64,
    expected_returned: u64,
) -> Result<u64> {
    let held_count =
        u64::try_from(held_ids.len()).context("held SKIP LOCKED row count does not fit u64")?;
    let returned_count = u64::try_from(returned_ids.len())
        .context("returned SKIP LOCKED row count does not fit u64")?;
    ensure!(
        held_count == expected_held,
        "{database} held {held_count} rows, expected exactly {expected_held}"
    );
    ensure!(
        returned_count == expected_returned,
        "{database} SKIP LOCKED returned {returned_count} rows, expected exactly {expected_returned}"
    );

    let held_set: HashSet<i64> = held_ids.iter().copied().collect();
    let returned_set: HashSet<i64> = returned_ids.iter().copied().collect();
    ensure!(
        held_set.len() == held_ids.len(),
        "{database} lock-holder query returned duplicate row identifiers"
    );
    ensure!(
        returned_set.len() == returned_ids.len(),
        "{database} SKIP LOCKED query returned duplicate row identifiers"
    );
    ensure!(
        held_set.is_disjoint(&returned_set),
        "{database} SKIP LOCKED returned one or more rows held by the other transaction"
    );
    Ok(returned_count)
}

fn json_field_equals(value: &serde_json::Value, field: &str, expected: &str) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.get(field).and_then(serde_json::Value::as_str) == Some(expected)
                || object
                    .values()
                    .any(|child| json_field_equals(child, field, expected))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|child| json_field_equals(child, field, expected)),
        _ => false,
    }
}

fn validate_count(database: &str, count: i64, expected: u64) -> Result<u64> {
    let count = u64::try_from(count).context("COUNT(*) returned a negative value")?;
    ensure!(
        count == expected,
        "{database} range COUNT(*) returned {count}, expected exactly {expected}; check that the table was populated with this configuration"
    );
    Ok(count)
}

fn query_report(
    sql: String,
    explain: ExplainReport,
    explain_range: QueryRange,
    config: &ResolvedConfig,
    observed_count: u64,
    warmup_ms: Vec<f64>,
    measured_queries: Vec<TimedRangeQueryReport>,
) -> QueryReport {
    let measured_ms = measured_queries
        .iter()
        .map(|query| query.elapsed_ms)
        .collect::<Vec<_>>();
    let summary_ms = summarize(&measured_ms);
    QueryReport {
        sql,
        connection_scope: "EXPLAIN, warmups, and measured runs use one acquired connection",
        explain,
        explain_range: "first measured query range",
        range_mode: config.query_ranges.as_str(),
        range_semantics: "event_time >= lower_bound AND event_time < upper_bound",
        lower_bound_utc: format_utc(explain_range.lower_bound),
        upper_bound_utc: format_utc(explain_range.upper_bound),
        expected_count: config.scan_rows,
        observed_count,
        warmup_ms,
        measured_ms,
        measured_queries,
        summary_ms,
    }
}

fn insert_report(
    rows: u64,
    batches: u64,
    transactions: u64,
    duration: StdDuration,
    generated_fingerprint: String,
) -> InsertReport {
    let seconds = duration.as_secs_f64();
    InsertReport {
        timing_scope: "row generation + fingerprint calculation + SQL execution + transaction commits + configured progress logging",
        includes_row_generation: true,
        includes_fingerprint_calculation: true,
        includes_progress_logging: true,
        rows,
        batches,
        transactions,
        elapsed_ms: elapsed_ms(duration),
        rows_per_second: if seconds == 0.0 {
            0.0
        } else {
            rows as f64 / seconds
        },
        generated_fingerprint,
    }
}

fn report_progress(
    database: &str,
    inserted: u64,
    total: u64,
    elapsed: StdDuration,
    interval: u64,
    next_progress: &mut u64,
) {
    if interval == 0 || (inserted < *next_progress && inserted != total) {
        return;
    }
    let rate = inserted as f64 / elapsed.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "{database}: inserted {inserted}/{total} rows ({:.2}%), {:.0} rows/s",
        inserted as f64 * 100.0 / total as f64,
        rate
    );
    while *next_progress <= inserted {
        *next_progress = next_progress.saturating_add(interval);
        if *next_progress == u64::MAX {
            break;
        }
    }
}

fn summarize(samples: &[f64]) -> TimingSummary {
    debug_assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let len = sorted.len();
    let mean = sorted.iter().sum::<f64>() / len as f64;
    let median = if len.is_multiple_of(2) {
        (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
    } else {
        sorted[len / 2]
    };
    let p95_index = ((len as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(len - 1);
    TimingSummary {
        min: sorted[0],
        max: sorted[len - 1],
        mean,
        median,
        p95: sorted[p95_index],
    }
}

fn print_console_summary(report: &DatabaseReport) {
    if let Some(insert) = report.insert.as_ref() {
        eprintln!(
            "{} summary: insert {:.3} ms, {:.0} rows/s",
            report.database, insert.elapsed_ms, insert.rows_per_second
        );
    } else {
        eprintln!("{} summary: insert skipped", report.database);
    }
    eprintln!(
        "{} summary: COUNT(*)={}, query ms min={:.3}, median={:.3}, p95={:.3}, max={:.3}",
        report.database,
        report.query.observed_count,
        report.query.summary_ms.min,
        report.query.summary_ms.median,
        report.query.summary_ms.p95,
        report.query.summary_ms.max
    );
    if let Some(skip_locked) = report.skip_locked.as_ref() {
        eprintln!(
            "{} summary: SKIP LOCKED candidates={}, held={}, returned={}, query ms={:.3}, event_time_index_used={}",
            report.database,
            skip_locked.candidate_rows_observed,
            skip_locked.held_rows_observed,
            skip_locked.returned_rows_observed,
            skip_locked.elapsed_ms,
            skip_locked.explain_uses_expected_index
        );
    } else {
        eprintln!("{} summary: SKIP LOCKED skipped", report.database);
    }
}

fn report_config(config: &ResolvedConfig) -> ReportConfig {
    ReportConfig {
        database: config.selection.as_str().to_owned(),
        table: config.table.clone(),
        rows: config.rows,
        scan_rows: config.scan_rows,
        skip_locked_rows: config.skip_locked_rows,
        skip_locked_held_rows: config.skip_locked_held_rows,
        range_start_row: config.range_start_row,
        batch_size: config.batch_size,
        transaction_rows: config.transaction_rows,
        warmups: config.warmups,
        measured_runs: config.runs,
        query_ranges: config.query_ranges.as_str(),
        pool_size: config.pool_size,
        progress_every: config.progress_every,
        seed: config.seed,
        base_time_utc: format_utc(config.base_time),
        skip_insert: config.skip_insert,
        skip_maintenance: config.skip_maintenance,
        skip_lock_test: config.skip_lock_test,
    }
}

fn data_model_report(config: &ResolvedConfig) -> Result<DataModelReport> {
    let columns = [
        ("id", "BIGINT", "row number + 1"),
        (
            "event_time",
            "TIMESTAMP(6)",
            "base_time + zero-based row number in seconds",
        ),
        ("user_id", "BIGINT", "deterministic integer in [1, 5000000]"),
        (
            "order_id",
            "BIGINT",
            "deterministic integer in [1000000000, 1899999999]",
        ),
        (
            "category_id",
            "INTEGER",
            "deterministic integer in [1, 1000]",
        ),
        ("status", "INTEGER", "deterministic integer in [0, 7]"),
        ("quantity", "INTEGER", "deterministic integer in [1, 20]"),
        ("score", "INTEGER", "deterministic integer in [0, 10000]"),
        ("region", "VARCHAR(16)", "one of 8 fixed region names"),
        ("device", "VARCHAR(16)", "one of 6 fixed device names"),
        (
            "customer_name",
            "VARCHAR(32)",
            "user_ plus 16 lowercase hexadecimal digits",
        ),
        (
            "email",
            "VARCHAR(64)",
            "u plus 16 lowercase hexadecimal digits plus @example.test",
        ),
        ("city", "VARCHAR(32)", "one of 12 fixed city names"),
        (
            "note",
            "VARCHAR(64)",
            "note- plus 16 lowercase hexadecimal digits",
        ),
        ("source", "VARCHAR(16)", "one of 6 fixed source names"),
    ]
    .into_iter()
    .map(|(name, logical_type, value_rule)| ColumnReport {
        name,
        logical_type,
        nullable: false,
        value_rule,
    })
    .collect();

    let sample_rows = [0, config.rows / 2, config.rows - 1]
        .into_iter()
        .map(|row_index| SampleRowReport::generate(row_index, config))
        .collect::<Result<Vec<_>>>()?;

    Ok(DataModelReport {
        generator: GENERATOR_VERSION,
        event_time_rule: "row N = base_time + N seconds (strictly increasing)",
        fingerprint_algorithm: FINGERPRINT_ALGORITHM,
        schema_status: if config.skip_insert {
            "expected schema only; existing table and indexes were not introspected"
        } else {
            "created by this run from the recorded column model"
        },
        sample_rows_source: "deterministic generator; not rows read back from the database",
        primary_key_column: "id",
        indexed_column: "event_time",
        index_created_before_insert: (!config.skip_insert).then_some(true),
        explicit_indexes: (!config.skip_insert).then_some(2),
        columns,
        sample_rows,
    })
}

fn write_report(path: &Path, report: &BenchmarkReport) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create result directory {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(report).context("serialize benchmark result")?;
    fs::write(path, json).with_context(|| format!("write result file {}", path.display()))?;
    Ok(())
}

fn event_time_at(base: NaiveDateTime, row: u64) -> Result<NaiveDateTime> {
    let seconds = i64::try_from(row).context("row number is too large for a timestamp offset")?;
    base.checked_add_signed(Duration::seconds(seconds))
        .context("event_time exceeded the supported timestamp range")
}

fn validate_identifier(identifier: &str) -> Result<()> {
    ensure!(
        !identifier.is_empty() && identifier.len() <= 40,
        "--table must contain between 1 and 40 ASCII characters"
    );
    let mut chars = identifier.chars();
    let first = chars.next().expect("identifier was checked as non-empty");
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!(
            "invalid --table {:?}; use an ASCII letter or underscore first, followed by letters, digits, or underscores",
            identifier
        );
    }
    Ok(())
}

fn mysql_identifier(identifier: &str) -> String {
    format!("`{identifier}`")
}

fn postgres_identifier(identifier: &str) -> String {
    format!("\"{identifier}\"")
}

fn format_utc(value: NaiveDateTime) -> String {
    value.and_utc().to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn elapsed_ms(duration: StdDuration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ResolvedConfig {
        ResolvedConfig::from_args(Args {
            database: DatabaseSelection::Both,
            mysql_url: String::new(),
            postgres_url: String::new(),
            table: "benchmark_events".to_owned(),
            rows: 30_000_000,
            scan_rows: 5_000_000,
            skip_locked_rows: 500,
            skip_locked_held_rows: 100,
            range_start_row: None,
            batch_size: 1_000,
            transaction_rows: 100_000,
            warmups: 2,
            runs: 5,
            query_ranges: QueryRangeMode::Same,
            pool_size: 1,
            seed: 20_260_715,
            base_time: "2024-01-01T00:00:00Z".to_owned(),
            progress_every: 1_000_000,
            skip_insert: false,
            skip_maintenance: false,
            skip_lock_test: false,
            output: PathBuf::from("benchmark-results/run.json"),
        })
        .expect("valid test config")
    }

    fn different_range_args() -> Args {
        Args {
            database: DatabaseSelection::Both,
            mysql_url: String::new(),
            postgres_url: String::new(),
            table: "benchmark_events".to_owned(),
            rows: 100,
            scan_rows: 20,
            skip_locked_rows: 500,
            skip_locked_held_rows: 100,
            range_start_row: None,
            batch_size: 10,
            transaction_rows: 100,
            warmups: 0,
            runs: 10,
            query_ranges: QueryRangeMode::Different,
            pool_size: 1,
            seed: 1,
            base_time: "2024-01-01T00:00:00Z".to_owned(),
            progress_every: 0,
            skip_insert: false,
            skip_maintenance: true,
            skip_lock_test: true,
            output: PathBuf::from("result.json"),
        }
    }

    #[test]
    fn generator_is_deterministic_and_time_increases_by_one_second() {
        let config = test_config();
        let first = BenchRow::generate(42, &config).expect("generate first copy");
        let second = BenchRow::generate(42, &config).expect("generate second copy");
        let next = BenchRow::generate(43, &config).expect("generate next row");

        assert_eq!(first.id, second.id);
        assert_eq!(first.event_time, second.event_time);
        assert_eq!(first.user_id, second.user_id);
        assert_eq!(first.order_id, second.order_id);
        assert_eq!(first.category_id, second.category_id);
        assert_eq!(first.status, second.status);
        assert_eq!(first.quantity, second.quantity);
        assert_eq!(first.score, second.score);
        assert_eq!(first.region, second.region);
        assert_eq!(first.device, second.device);
        assert_eq!(first.customer_name, second.customer_name);
        assert_eq!(first.email, second.email);
        assert_eq!(first.city, second.city);
        assert_eq!(first.note, second.note);
        assert_eq!(first.source, second.source);
        assert_eq!(
            next.event_time - first.event_time,
            Duration::seconds(1),
            "event_time must increase exactly one second per row"
        );
    }

    #[test]
    fn default_range_is_centered_left_closed_right_open_and_exact() {
        let config = test_config();
        assert_eq!(config.range_start_row, 12_500_000);
        let range_end_row = config.range_start_row + config.scan_rows;
        assert_eq!(range_end_row, 17_500_000);
        assert_eq!(
            config.upper_bound - config.lower_bound,
            Duration::seconds(5_000_000)
        );
        // With one timestamp per second, [start, end) includes exactly end-start rows.
        assert_eq!(range_end_row - config.range_start_row, 5_000_000);
        let is_in_range = |row: u64| row >= config.range_start_row && row < range_end_row;
        assert!(!is_in_range(config.range_start_row - 1));
        assert!(is_in_range(config.range_start_row));
        assert!(is_in_range(range_end_row - 1));
        assert!(!is_in_range(range_end_row));
    }

    #[test]
    fn default_skip_locked_range_is_exactly_five_hundred_rows() {
        let config = test_config();
        assert_eq!(config.skip_locked_rows, 500);
        assert_eq!(config.skip_locked_held_rows, 100);
        assert_eq!(
            config.skip_locked_upper_bound - config.lower_bound,
            Duration::seconds(500)
        );
        assert_eq!(config.skip_locked_rows - config.skip_locked_held_rows, 400);
    }

    #[test]
    fn skip_locked_validation_requires_exact_disjoint_unique_rows() {
        let held = [1, 2];
        let returned = [3, 4, 5];
        assert_eq!(
            validate_skip_locked_ids("test", &held, &returned, 2, 3)
                .expect("valid SKIP LOCKED result"),
            3
        );
        assert!(
            validate_skip_locked_ids("test", &held, &[2, 3, 4], 2, 3).is_err(),
            "a held row must never be returned"
        );
        assert!(
            validate_skip_locked_ids("test", &held, &[3, 3, 4], 2, 3).is_err(),
            "duplicate returned identifiers must fail"
        );
        assert!(
            validate_skip_locked_ids("test", &held, &[3, 4], 2, 3).is_err(),
            "the returned count must be exact"
        );
    }

    #[test]
    fn explain_index_detection_uses_the_actual_plan_field() {
        let mysql_plan = serde_json::json!({
            "query_block": {
                "table": {
                    "possible_keys": ["idx_benchmark_events_event_time"],
                    "key": "another_index"
                }
            }
        });
        assert!(!json_field_equals(
            &mysql_plan,
            "key",
            "idx_benchmark_events_event_time"
        ));

        let postgres_plan = serde_json::json!([{
            "Plan": {
                "Node Type": "Index Scan",
                "Index Name": "idx_benchmark_events_event_time"
            }
        }]);
        assert!(json_field_equals(
            &postgres_plan,
            "Index Name",
            "idx_benchmark_events_event_time"
        ));
    }

    #[test]
    fn reported_model_has_fifteen_not_null_columns_id_primary_key_and_time_index() {
        let config = test_config();
        let model = data_model_report(&config).expect("build data-model report");

        assert_eq!(model.columns.len(), 15);
        assert!(model.columns.iter().all(|column| !column.nullable));
        assert_eq!(model.primary_key_column, "id");
        assert_eq!(model.explicit_indexes, Some(2));
        assert_eq!(model.indexed_column, "event_time");
        assert_eq!(model.index_created_before_insert, Some(true));
        assert_eq!(model.sample_rows.len(), 3);
    }

    #[test]
    fn query_once_mode_is_recorded_without_warmups_or_extra_work() {
        let mut config = test_config();
        config.skip_insert = true;
        config.skip_maintenance = true;
        config.skip_lock_test = true;
        config.warmups = 0;
        config.runs = 1;

        let report = report_config(&config);
        assert!(report.skip_insert);
        assert!(report.skip_maintenance);
        assert!(report.skip_lock_test);
        assert_eq!(report.warmups, 0);
        assert_eq!(report.measured_runs, 1);
        assert_eq!(report.query_ranges, "same");
    }

    #[test]
    fn different_ranges_are_unique_evenly_spread_and_unwarmed() {
        let config =
            ResolvedConfig::from_args(different_range_args()).expect("valid different ranges");
        let ranges = (0..config.runs)
            .map(|index| measured_query_range(&config, index).expect("valid measured range"))
            .collect::<Vec<_>>();
        let starts = ranges
            .iter()
            .map(|range| range.start_row)
            .collect::<Vec<_>>();

        assert_eq!(starts, [0, 8, 17, 26, 35, 44, 53, 62, 71, 80]);
        assert_eq!(
            starts.iter().copied().collect::<HashSet<_>>().len(),
            config.runs as usize
        );
        assert!(
            ranges
                .iter()
                .all(|range| range.end_row - range.start_row == config.scan_rows)
        );
        assert_eq!(config.warmups, 0);
    }

    #[test]
    fn different_ranges_reject_warmups() {
        let mut args = different_range_args();
        args.warmups = 1;
        let error = ResolvedConfig::from_args(args).expect_err("warmups must be rejected");
        assert!(
            error
                .to_string()
                .contains("--query-ranges different requires --warmups 0")
        );
    }

    #[test]
    fn skipped_lock_test_does_not_limit_a_smaller_query_range() {
        let config = ResolvedConfig::from_args(Args {
            database: DatabaseSelection::Both,
            mysql_url: String::new(),
            postgres_url: String::new(),
            table: "benchmark_events".to_owned(),
            rows: 10,
            scan_rows: 1,
            skip_locked_rows: 500,
            skip_locked_held_rows: 100,
            range_start_row: None,
            batch_size: 10,
            transaction_rows: 10,
            warmups: 0,
            runs: 1,
            query_ranges: QueryRangeMode::Same,
            pool_size: 1,
            seed: 1,
            base_time: "2024-01-01T00:00:00Z".to_owned(),
            progress_every: 0,
            skip_insert: true,
            skip_maintenance: true,
            skip_lock_test: true,
            output: PathBuf::from("result.json"),
        })
        .expect("disabled lock test should ignore its unused range parameters");

        assert_eq!(config.scan_rows, 1);
        assert_eq!(config.skip_locked_upper_bound, config.lower_bound);
    }

    #[test]
    fn batch_size_respects_cross_database_parameter_limit() {
        let mut args = Args {
            database: DatabaseSelection::Both,
            mysql_url: String::new(),
            postgres_url: String::new(),
            table: "benchmark_events".to_owned(),
            rows: 10,
            scan_rows: 5,
            skip_locked_rows: 5,
            skip_locked_held_rows: 1,
            range_start_row: None,
            batch_size: MAX_BIND_PARAMETERS / FIELD_COUNT + 1,
            transaction_rows: 10,
            warmups: 0,
            runs: 1,
            query_ranges: QueryRangeMode::Same,
            pool_size: 1,
            seed: 1,
            base_time: "2024-01-01T00:00:00Z".to_owned(),
            progress_every: 0,
            skip_insert: false,
            skip_maintenance: false,
            skip_lock_test: false,
            output: PathBuf::from("result.json"),
        };
        assert!(ResolvedConfig::from_args(args).is_err());

        args = Args {
            database: DatabaseSelection::Both,
            mysql_url: String::new(),
            postgres_url: String::new(),
            table: "benchmark_events".to_owned(),
            rows: 10,
            scan_rows: 5,
            skip_locked_rows: 5,
            skip_locked_held_rows: 1,
            range_start_row: None,
            batch_size: MAX_BIND_PARAMETERS / FIELD_COUNT,
            transaction_rows: 10,
            warmups: 0,
            runs: 1,
            query_ranges: QueryRangeMode::Same,
            pool_size: 1,
            seed: 1,
            base_time: "2024-01-01T00:00:00Z".to_owned(),
            progress_every: 0,
            skip_insert: false,
            skip_maintenance: false,
            skip_lock_test: false,
            output: PathBuf::from("result.json"),
        };
        assert!(ResolvedConfig::from_args(args).is_ok());
    }
}
