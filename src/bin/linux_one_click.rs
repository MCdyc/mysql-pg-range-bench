use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use serde::Serialize;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

const DEFAULT_ROWS: u64 = 30_000_000;
const DEFAULT_SCAN_ROWS: u64 = 5_000_000;
const DEFAULT_SKIP_LOCKED_ROWS: u64 = 500;
const DEFAULT_SKIP_LOCKED_HELD_ROWS: u64 = 100;
const DATABASE_PREFIX: &str = "codex_range_bench_";
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Run the MySQL/PostgreSQL benchmark in random temporary databases, then remove them"
)]
struct Args {
    /// Administrator MySQL URL whose database is retained. Never written to receipts.
    #[arg(long, env = "MYSQL_ADMIN_URL", hide_env_values = true)]
    mysql_admin_url: String,

    /// Administrator PostgreSQL URL whose database is retained. Never written to receipts.
    #[arg(long, env = "POSTGRES_ADMIN_URL", hide_env_values = true)]
    postgres_admin_url: String,

    /// Number of deterministic rows inserted into each temporary database.
    #[arg(long, env = "BENCH_ROWS", default_value_t = DEFAULT_ROWS)]
    rows: u64,

    /// Number of rows covered by the indexed range query.
    #[arg(long, env = "BENCH_SCAN_ROWS", default_value_t = DEFAULT_SCAN_ROWS)]
    scan_rows: u64,

    /// Rows in the indexed SKIP LOCKED candidate range.
    #[arg(
        long,
        env = "BENCH_SKIP_LOCKED_ROWS",
        default_value_t = DEFAULT_SKIP_LOCKED_ROWS
    )]
    skip_locked_rows: u64,

    /// Leading candidate rows held by another transaction.
    #[arg(
        long,
        env = "BENCH_SKIP_LOCKED_HELD_ROWS",
        default_value_t = DEFAULT_SKIP_LOCKED_HELD_ROWS
    )]
    skip_locked_held_rows: u64,

    /// Rows per multi-value insert.
    #[arg(long, env = "BENCH_BATCH_SIZE", default_value_t = 1_000)]
    batch_size: usize,

    /// Maximum rows committed in one transaction.
    #[arg(long, env = "BENCH_TRANSACTION_ROWS", default_value_t = 100_000)]
    transaction_rows: u64,

    /// Warm-up queries per database.
    #[arg(long, env = "BENCH_WARMUPS", default_value_t = 2)]
    warmups: u32,

    /// Measured queries per database.
    #[arg(long, env = "BENCH_RUNS", default_value_t = 5)]
    runs: u32,

    /// Insertion progress interval. Zero disables progress messages.
    #[arg(long, env = "BENCH_PROGRESS_EVERY", default_value_t = 1_000_000)]
    progress_every: u64,

    /// Benchmark result JSON path.
    #[arg(
        long,
        env = "BENCH_OUTPUT",
        default_value = "benchmark-results/linux-run.json"
    )]
    output: PathBuf,

    /// Cleanup receipt JSON path. Defaults to a sibling of --output.
    #[arg(long, env = "BENCH_CLEANUP_RECEIPT")]
    cleanup_receipt: Option<PathBuf>,
}

#[derive(Debug)]
struct Config {
    mysql_admin_url: String,
    postgres_admin_url: String,
    rows: u64,
    scan_rows: u64,
    skip_locked_rows: u64,
    skip_locked_held_rows: u64,
    batch_size: usize,
    transaction_rows: u64,
    warmups: u32,
    runs: u32,
    progress_every: u64,
    output: PathBuf,
    cleanup_receipt: PathBuf,
}

impl Config {
    fn from_args(args: Args) -> Result<Self> {
        ensure!(args.rows > 0, "--rows must be greater than zero");
        ensure!(
            args.scan_rows > 0 && args.scan_rows <= args.rows,
            "--scan-rows must be between 1 and --rows"
        );
        ensure!(
            args.skip_locked_rows > 1 && args.skip_locked_rows <= args.scan_rows,
            "--skip-locked-rows must be between 2 and --scan-rows"
        );
        ensure!(
            args.skip_locked_held_rows > 0 && args.skip_locked_held_rows < args.skip_locked_rows,
            "--skip-locked-held-rows must be between 1 and --skip-locked-rows - 1"
        );
        ensure!(
            args.batch_size > 0,
            "--batch-size must be greater than zero"
        );
        ensure!(
            args.transaction_rows > 0,
            "--transaction-rows must be greater than zero"
        );
        ensure!(args.runs > 0, "--runs must be greater than zero");

        validate_admin_url(&args.mysql_admin_url, "mysql")?;
        validate_admin_url(&args.postgres_admin_url, "postgres")?;

        let cleanup_receipt = args
            .cleanup_receipt
            .unwrap_or_else(|| derived_receipt_path(&args.output));
        ensure!(
            args.output != cleanup_receipt,
            "--output and --cleanup-receipt must be different paths"
        );

        Ok(Self {
            mysql_admin_url: args.mysql_admin_url,
            postgres_admin_url: args.postgres_admin_url,
            rows: args.rows,
            scan_rows: args.scan_rows,
            skip_locked_rows: args.skip_locked_rows,
            skip_locked_held_rows: args.skip_locked_held_rows,
            batch_size: args.batch_size,
            transaction_rows: args.transaction_rows,
            warmups: args.warmups,
            runs: args.runs,
            progress_every: args.progress_every,
            output: args.output,
            cleanup_receipt,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptStatus {
    Initial,
    DatabasesCreated,
    BenchmarkRunning,
    CleanupFinished,
    CleanupIncomplete,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CleanupStatus {
    Pending,
    NotCreated,
    DeletedAndVerified,
    FailedOrUnverified,
}

#[derive(Debug, Serialize)]
struct DatabaseReceipt {
    name: String,
    create_attempted: bool,
    created_by_this_run: bool,
    cleanup_authorized: bool,
    cleanup_status: CleanupStatus,
    exists_after_cleanup: Option<bool>,
    cleanup_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CleanupReceipt {
    receipt_version: u32,
    run_id: String,
    started_at_utc: String,
    updated_at_utc: String,
    status: ReceiptStatus,
    benchmark_exit_code: Option<i32>,
    shutdown_signal: Option<&'static str>,
    mysql: DatabaseReceipt,
    postgres: DatabaseReceipt,
    cleanup_complete: bool,
    limitation: &'static str,
}

impl CleanupReceipt {
    fn new(run_id: String, database_name: String) -> Self {
        let now = now_utc();
        Self {
            receipt_version: 1,
            run_id,
            started_at_utc: now.clone(),
            updated_at_utc: now,
            status: ReceiptStatus::Initial,
            benchmark_exit_code: None,
            shutdown_signal: None,
            mysql: DatabaseReceipt {
                name: database_name.clone(),
                create_attempted: false,
                created_by_this_run: false,
                cleanup_authorized: false,
                cleanup_status: CleanupStatus::Pending,
                exists_after_cleanup: None,
                cleanup_error: None,
            },
            postgres: DatabaseReceipt {
                name: database_name,
                create_attempted: false,
                created_by_this_run: false,
                cleanup_authorized: false,
                cleanup_status: CleanupStatus::Pending,
                exists_after_cleanup: None,
                cleanup_error: None,
            },
            cleanup_complete: false,
            limitation: "SIGKILL, sudden power loss, or an operating-system crash cannot run automatic cleanup; use the exact names in this receipt for recovery",
        }
    }

    fn touch(&mut self) {
        self.updated_at_utc = now_utc();
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))]
enum ShutdownSignal {
    Interrupt,
    Terminate,
    Hangup,
}

impl ShutdownSignal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "sigint",
            Self::Terminate => "sigterm",
            Self::Hangup => "sighup",
        }
    }
}

#[derive(Debug)]
enum BenchmarkOutcome {
    Exited(ExitStatus),
    Interrupted(ShutdownSignal),
}

#[derive(Debug)]
struct CleanupResult {
    status: CleanupStatus,
    exists_after_cleanup: Option<bool>,
    error: Option<String>,
}

#[tokio::main]
async fn main() {
    if Path::new(".env").exists()
        && let Err(error) = dotenvy::dotenv().context("load .env from the current directory")
    {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }

    let config = match Config::from_args(Args::parse()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(2);
        }
    };

    match run(config).await {
        Ok(BenchmarkOutcome::Exited(status)) if status.success() => {}
        Ok(BenchmarkOutcome::Exited(status)) => {
            eprintln!("benchmark exited unsuccessfully: {status}");
            std::process::exit(status.code().unwrap_or(1));
        }
        Ok(BenchmarkOutcome::Interrupted(signal)) => {
            eprintln!(
                "benchmark interrupted by {}; temporary database cleanup was attempted",
                signal.as_str()
            );
            std::process::exit(match signal {
                ShutdownSignal::Interrupt => 130,
                ShutdownSignal::Terminate => 143,
                ShutdownSignal::Hangup => 129,
            });
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

async fn run(config: Config) -> Result<BenchmarkOutcome> {
    prepare_fresh_artifact_paths(&config.output, &config.cleanup_receipt)?;
    eprintln!("preflight: connecting to both administrator databases...");
    let (mysql_pool, postgres_pool) = connect_admin_databases(&config).await?;
    let (run_id, database_name) = choose_unused_database_name(&mysql_pool, &postgres_pool).await?;
    let mysql_benchmark_url =
        replace_database_path(&config.mysql_admin_url, &database_name, "mysql")?;
    let postgres_benchmark_url =
        replace_database_path(&config.postgres_admin_url, &database_name, "postgres")?;

    // Installing handlers and allowing their task to be polled happens before the
    // initial receipt and, critically, before either CREATE DATABASE statement.
    let mut shutdown_rx = install_shutdown_listener()?;
    tokio::task::yield_now().await;

    let mut receipt = CleanupReceipt::new(run_id, database_name.clone());
    write_initial_receipt(&config.cleanup_receipt, &mut receipt)?;
    eprintln!(
        "cleanup receipt initialized at {}",
        config.cleanup_receipt.display()
    );

    let operation_result = create_and_run(
        &config,
        &mysql_pool,
        &postgres_pool,
        &mysql_benchmark_url,
        &postgres_benchmark_url,
        &mut shutdown_rx,
        &mut receipt,
    )
    .await;

    eprintln!("cleanup: removing this run's exact temporary database names...");
    let (mysql_cleanup, postgres_cleanup) = tokio::join!(
        cleanup_mysql(
            &mysql_pool,
            &database_name,
            receipt.mysql.cleanup_authorized
        ),
        cleanup_postgres(
            &postgres_pool,
            &database_name,
            receipt.postgres.cleanup_authorized
        )
    );
    apply_cleanup_result(&mut receipt.mysql, mysql_cleanup);
    apply_cleanup_result(&mut receipt.postgres, postgres_cleanup);
    receipt.cleanup_complete = matches!(
        receipt.mysql.cleanup_status,
        CleanupStatus::DeletedAndVerified | CleanupStatus::NotCreated
    ) && matches!(
        receipt.postgres.cleanup_status,
        CleanupStatus::DeletedAndVerified | CleanupStatus::NotCreated
    );
    receipt.status = if receipt.cleanup_complete {
        ReceiptStatus::CleanupFinished
    } else {
        ReceiptStatus::CleanupIncomplete
    };
    write_receipt(&config.cleanup_receipt, &mut receipt)?;

    if !receipt.cleanup_complete {
        bail!(
            "cleanup could not be verified for every database; inspect {} and remove only the exact recorded names",
            config.cleanup_receipt.display()
        );
    }

    operation_result
}

async fn connect_admin_databases(config: &Config) -> Result<(MySqlPool, PgPool)> {
    let mysql_pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&config.mysql_admin_url)
        .await
        .context("connect to the MySQL administrator database")?;
    let mysql_database: Option<String> = sqlx::query_scalar("SELECT DATABASE()")
        .fetch_one(&mysql_pool)
        .await
        .context("run MySQL administrator preflight")?;
    ensure!(
        mysql_database.is_some(),
        "MYSQL_ADMIN_URL must select an existing administrator database"
    );

    let postgres_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&config.postgres_admin_url)
        .await
        .context("connect to the PostgreSQL administrator database")?;
    let postgres_database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&postgres_pool)
        .await
        .context("run PostgreSQL administrator preflight")?;
    ensure!(
        !postgres_database.is_empty(),
        "POSTGRES_ADMIN_URL must select an existing administrator database"
    );

    Ok((mysql_pool, postgres_pool))
}

async fn choose_unused_database_name(
    mysql_pool: &MySqlPool,
    postgres_pool: &PgPool,
) -> Result<(String, String)> {
    for _ in 0..8 {
        let run_id = Uuid::new_v4().simple().to_string();
        let database_name = format!("{DATABASE_PREFIX}{run_id}");
        let (mysql_exists, postgres_exists) = tokio::try_join!(
            mysql_database_exists(mysql_pool, &database_name),
            postgres_database_exists(postgres_pool, &database_name)
        )?;
        if !mysql_exists && !postgres_exists {
            return Ok((run_id, database_name));
        }
    }
    bail!("could not find an unused cryptographically random temporary database name")
}

async fn create_and_run(
    config: &Config,
    mysql_pool: &MySqlPool,
    postgres_pool: &PgPool,
    mysql_benchmark_url: &str,
    postgres_benchmark_url: &str,
    shutdown_rx: &mut mpsc::UnboundedReceiver<ShutdownSignal>,
    receipt: &mut CleanupReceipt,
) -> Result<BenchmarkOutcome> {
    if let Ok(signal) = shutdown_rx.try_recv() {
        receipt.shutdown_signal = Some(signal.as_str());
        return Ok(BenchmarkOutcome::Interrupted(signal));
    }

    let mysql_create = format!(
        "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        mysql_identifier(&receipt.mysql.name)
    );
    // The name was verified absent and cannot be user supplied. Persist cleanup
    // authorization before CREATE so an ambiguous network error cannot leave a
    // server-side database behind with created_by_this_run still false.
    receipt.mysql.create_attempted = true;
    receipt.mysql.cleanup_authorized = true;
    write_receipt(&config.cleanup_receipt, receipt)?;
    sqlx::query(&mysql_create)
        .execute(mysql_pool)
        .await
        .context("create the exact random MySQL temporary database")?;
    receipt.mysql.created_by_this_run = true;
    write_receipt(&config.cleanup_receipt, receipt)?;

    if let Ok(signal) = shutdown_rx.try_recv() {
        receipt.shutdown_signal = Some(signal.as_str());
        return Ok(BenchmarkOutcome::Interrupted(signal));
    }

    let postgres_create = format!(
        "CREATE DATABASE {} ENCODING 'UTF8' TEMPLATE template0",
        postgres_identifier(&receipt.postgres.name)
    );
    receipt.postgres.create_attempted = true;
    receipt.postgres.cleanup_authorized = true;
    write_receipt(&config.cleanup_receipt, receipt)?;
    sqlx::query(&postgres_create)
        .execute(postgres_pool)
        .await
        .context("create the exact random PostgreSQL temporary database")?;
    receipt.postgres.created_by_this_run = true;
    receipt.status = ReceiptStatus::DatabasesCreated;
    write_receipt(&config.cleanup_receipt, receipt)?;

    if let Ok(signal) = shutdown_rx.try_recv() {
        receipt.shutdown_signal = Some(signal.as_str());
        return Ok(BenchmarkOutcome::Interrupted(signal));
    }

    let benchmark_bin = sibling_benchmark_binary()?;
    receipt.status = ReceiptStatus::BenchmarkRunning;
    write_receipt(&config.cleanup_receipt, receipt)?;
    eprintln!("benchmark: starting isolated child process...");
    let mut child = benchmark_command(
        &benchmark_bin,
        config,
        mysql_benchmark_url,
        postgres_benchmark_url,
    )
    .spawn()
    .with_context(|| {
        format!(
            "start sibling benchmark executable {}",
            benchmark_bin.display()
        )
    })?;

    let outcome = tokio::select! {
        status = child.wait() => {
            BenchmarkOutcome::Exited(status.context("wait for benchmark child process")?)
        }
        signal = shutdown_rx.recv() => {
            let signal = signal.ok_or_else(|| anyhow!("shutdown signal listener stopped unexpectedly"))?;
            receipt.shutdown_signal = Some(signal.as_str());
            stop_child(&mut child).await?;
            BenchmarkOutcome::Interrupted(signal)
        }
    };

    if let BenchmarkOutcome::Exited(status) = &outcome {
        receipt.benchmark_exit_code = status.code();
    }
    Ok(outcome)
}

fn benchmark_command(
    benchmark_bin: &Path,
    config: &Config,
    mysql_url: &str,
    postgres_url: &str,
) -> Command {
    let mut command = Command::new(benchmark_bin);
    command
        .kill_on_drop(true)
        .env_remove("MYSQL_ADMIN_URL")
        .env_remove("POSTGRES_ADMIN_URL")
        .env("MYSQL_URL", mysql_url)
        .env("POSTGRES_URL", postgres_url)
        .env("BENCH_DATABASE", "both")
        .env("BENCH_ROWS", config.rows.to_string())
        .env("BENCH_SCAN_ROWS", config.scan_rows.to_string())
        .env(
            "BENCH_SKIP_LOCKED_ROWS",
            config.skip_locked_rows.to_string(),
        )
        .env(
            "BENCH_SKIP_LOCKED_HELD_ROWS",
            config.skip_locked_held_rows.to_string(),
        )
        .env("BENCH_BATCH_SIZE", config.batch_size.to_string())
        .env(
            "BENCH_TRANSACTION_ROWS",
            config.transaction_rows.to_string(),
        )
        .env("BENCH_WARMUPS", config.warmups.to_string())
        .env("BENCH_RUNS", config.runs.to_string())
        .env("BENCH_PROGRESS_EVERY", config.progress_every.to_string())
        .env("BENCH_OUTPUT", &config.output);
    command
}

async fn stop_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("check benchmark child status before stopping")?
        .is_some()
    {
        return Ok(());
    }
    child
        .start_kill()
        .context("request benchmark child termination")?;
    tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, child.wait())
        .await
        .context("time out waiting for benchmark child termination")?
        .context("wait for terminated benchmark child")?;
    Ok(())
}

async fn cleanup_mysql(
    pool: &MySqlPool,
    database_name: &str,
    cleanup_authorized: bool,
) -> CleanupResult {
    if !cleanup_authorized {
        return verify_not_created_mysql(pool, database_name).await;
    }

    let drop_sql = format!(
        "DROP DATABASE IF EXISTS {}",
        mysql_identifier(database_name)
    );
    let drop_failed = sqlx::query(&drop_sql).execute(pool).await.is_err();
    match mysql_database_exists(pool, database_name).await {
        Ok(false) => CleanupResult {
            status: CleanupStatus::DeletedAndVerified,
            exists_after_cleanup: Some(false),
            error: drop_failed.then_some(
                "DROP DATABASE returned an error, but the system catalog verifies absence"
                    .to_owned(),
            ),
        },
        Ok(true) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: Some(true),
            error: Some("exact temporary MySQL database still exists after DROP".to_owned()),
        },
        Err(_) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: None,
            error: Some("could not verify MySQL cleanup in information_schema.schemata".to_owned()),
        },
    }
}

async fn verify_not_created_mysql(pool: &MySqlPool, database_name: &str) -> CleanupResult {
    match mysql_database_exists(pool, database_name).await {
        Ok(false) => CleanupResult {
            status: CleanupStatus::NotCreated,
            exists_after_cleanup: Some(false),
            error: None,
        },
        Ok(true) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: Some(true),
            error: Some(
                "random MySQL database exists although creation was not recorded; it was not deleted"
                    .to_owned(),
            ),
        },
        Err(_) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: None,
            error: Some(
                "could not verify uncreated MySQL name in information_schema.schemata".to_owned(),
            ),
        },
    }
}

async fn cleanup_postgres(
    pool: &PgPool,
    database_name: &str,
    cleanup_authorized: bool,
) -> CleanupResult {
    if !cleanup_authorized {
        return verify_not_created_postgres(pool, database_name).await;
    }

    // Avoid PostgreSQL 13's DROP DATABASE ... WITH (FORCE) so the cleanup
    // remains compatible with older supported servers. First prevent new
    // sessions, then terminate only sessions attached to this exact random
    // database. Either preparatory statement may be unnecessary (for example,
    // when an ambiguous CREATE never reached the server), so the final catalog
    // verification remains the source of truth.
    let disallow_connections_sql = format!(
        "ALTER DATABASE {} WITH ALLOW_CONNECTIONS false",
        postgres_identifier(database_name)
    );
    let _ = sqlx::query(&disallow_connections_sql).execute(pool).await;
    let _ = sqlx::query_scalar::<_, bool>(
        "SELECT pg_catalog.pg_terminate_backend(pid) \
         FROM pg_catalog.pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_catalog.pg_backend_pid()",
    )
    .bind(database_name)
    .fetch_all(pool)
    .await;

    let drop_sql = format!(
        "DROP DATABASE IF EXISTS {}",
        postgres_identifier(database_name)
    );
    let drop_failed = sqlx::query(&drop_sql).execute(pool).await.is_err();
    match postgres_database_exists(pool, database_name).await {
        Ok(false) => CleanupResult {
            status: CleanupStatus::DeletedAndVerified,
            exists_after_cleanup: Some(false),
            error: drop_failed.then_some(
                "DROP DATABASE returned an error, but pg_catalog.pg_database verifies absence"
                    .to_owned(),
            ),
        },
        Ok(true) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: Some(true),
            error: Some("exact temporary PostgreSQL database still exists after DROP".to_owned()),
        },
        Err(_) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: None,
            error: Some("could not verify PostgreSQL cleanup in pg_catalog.pg_database".to_owned()),
        },
    }
}

async fn verify_not_created_postgres(pool: &PgPool, database_name: &str) -> CleanupResult {
    match postgres_database_exists(pool, database_name).await {
        Ok(false) => CleanupResult {
            status: CleanupStatus::NotCreated,
            exists_after_cleanup: Some(false),
            error: None,
        },
        Ok(true) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: Some(true),
            error: Some(
                "random PostgreSQL database exists although creation was not recorded; it was not deleted"
                    .to_owned(),
            ),
        },
        Err(_) => CleanupResult {
            status: CleanupStatus::FailedOrUnverified,
            exists_after_cleanup: None,
            error: Some(
                "could not verify uncreated PostgreSQL name in pg_catalog.pg_database".to_owned(),
            ),
        },
    }
}

fn apply_cleanup_result(receipt: &mut DatabaseReceipt, result: CleanupResult) {
    receipt.cleanup_status = result.status;
    receipt.exists_after_cleanup = result.exists_after_cleanup;
    receipt.cleanup_error = result.error;
}

async fn mysql_database_exists(pool: &MySqlPool, database_name: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(database_name)
    .fetch_one(pool)
    .await
    .context("query MySQL system catalog for an exact database name")?;
    Ok(count != 0)
}

async fn postgres_database_exists(pool: &PgPool, database_name: &str) -> Result<bool> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
    )
    .bind(database_name)
    .fetch_one(pool)
    .await
    .context("query PostgreSQL system catalog for an exact database name")?;
    Ok(exists)
}

fn install_shutdown_listener() -> Result<mpsc::UnboundedReceiver<ShutdownSignal>> {
    let (sender, receiver) = mpsc::unbounded_channel();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut interrupt =
            signal(SignalKind::interrupt()).context("install Unix SIGINT handler")?;
        let mut terminate =
            signal(SignalKind::terminate()).context("install Unix SIGTERM handler")?;
        let mut hangup = signal(SignalKind::hangup()).context("install Unix SIGHUP handler")?;
        tokio::spawn(async move {
            loop {
                let caught = tokio::select! {
                    value = interrupt.recv() => value.map(|_| ShutdownSignal::Interrupt),
                    value = terminate.recv() => value.map(|_| ShutdownSignal::Terminate),
                    value = hangup.recv() => value.map(|_| ShutdownSignal::Hangup),
                };
                match caught {
                    Some(signal) if sender.send(signal).is_ok() => {}
                    _ => break,
                }
            }
        });
    }

    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if sender.send(ShutdownSignal::Interrupt).is_err() {
                    break;
                }
            }
        });
    }

    Ok(receiver)
}

fn validate_admin_url(value: &str, expected_scheme: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| {
        format!("parse {expected_scheme} administrator URL (the URL value is intentionally hidden)")
    })?;
    ensure!(
        url.scheme() == expected_scheme,
        "{expected_scheme} administrator URL must use the {expected_scheme} scheme"
    );
    ensure!(
        url.host().is_some(),
        "{expected_scheme} administrator URL must include a host"
    );
    let database = url.path().trim_matches('/');
    ensure!(
        !database.is_empty() && !database.contains('/'),
        "{expected_scheme} administrator URL must select exactly one existing database"
    );
    ensure!(
        url.fragment().is_none(),
        "{expected_scheme} administrator URL must not contain a fragment"
    );
    Ok(())
}

fn replace_database_path(
    value: &str,
    database_name: &str,
    expected_scheme: &str,
) -> Result<String> {
    validate_admin_url(value, expected_scheme)?;
    validate_generated_database_name(database_name)?;
    let mut url =
        Url::parse(value).with_context(|| format!("parse {expected_scheme} administrator URL"))?;
    url.set_path(&format!("/{database_name}"));
    Ok(url.into())
}

fn validate_generated_database_name(database_name: &str) -> Result<()> {
    let suffix = database_name.strip_prefix(DATABASE_PREFIX);
    ensure!(
        suffix.is_some_and(|value| {
            value.len() == 32
                && value.chars().all(|character| {
                    character.is_ascii_hexdigit() && !character.is_ascii_uppercase()
                })
        }),
        "internal error: invalid generated database name"
    );
    Ok(())
}

fn sibling_benchmark_binary() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("locate one-click executable")?;
    let directory = current_exe
        .parent()
        .context("one-click executable has no parent directory")?;
    let mut filename = OsString::from("mysql-pg-range-bench");
    filename.push(std::env::consts::EXE_SUFFIX);
    let benchmark = directory.join(filename);
    ensure!(
        benchmark.is_file(),
        "sibling benchmark executable was not found at {}; build both binaries first",
        benchmark.display()
    );
    Ok(benchmark)
}

fn mysql_identifier(identifier: &str) -> String {
    format!("`{identifier}`")
}

fn postgres_identifier(identifier: &str) -> String {
    format!("\"{identifier}\"")
}

fn derived_receipt_path(output: &Path) -> PathBuf {
    let mut value = output.as_os_str().to_os_string();
    value.push(".cleanup.json");
    PathBuf::from(value)
}

fn prepare_fresh_artifact_paths(output: &Path, receipt: &Path) -> Result<()> {
    ensure!(
        output.file_name().is_some(),
        "--output must end with a file name"
    );
    ensure!(
        receipt.file_name().is_some(),
        "--cleanup-receipt must end with a file name"
    );

    let output_parent = effective_parent(output);
    let receipt_parent = effective_parent(receipt);
    fs::create_dir_all(output_parent)
        .with_context(|| format!("create result directory {}", output_parent.display()))?;
    fs::create_dir_all(receipt_parent).with_context(|| {
        format!(
            "create cleanup receipt directory {}",
            receipt_parent.display()
        )
    })?;

    let resolved_output = fs::canonicalize(output_parent)
        .with_context(|| format!("resolve result directory {}", output_parent.display()))?
        .join(output.file_name().expect("validated output file name"));
    let resolved_receipt = fs::canonicalize(receipt_parent)
        .with_context(|| format!("resolve receipt directory {}", receipt_parent.display()))?
        .join(receipt.file_name().expect("validated receipt file name"));
    ensure!(
        !paths_equal_for_platform(&resolved_output, &resolved_receipt),
        "--output and --cleanup-receipt resolve to the same file"
    );

    ensure_path_does_not_exist(output, "--output")?;
    ensure_path_does_not_exist(receipt, "--cleanup-receipt")?;
    Ok(())
}

#[cfg(windows)]
fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    left == right
}

fn ensure_path_does_not_exist(path: &Path, option: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "{option} must name a fresh file that does not already exist: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {option} path {}", path.display()))
        }
    }
}

fn write_initial_receipt(path: &Path, receipt: &mut CleanupReceipt) -> Result<()> {
    prepare_receipt_parent(path)?;
    receipt.touch();
    let json = serde_json::to_vec_pretty(receipt).context("serialize initial cleanup receipt")?;
    write_new_private_file(path, &json).with_context(|| {
        format!(
            "create initial cleanup receipt {}; choose a fresh --output or --cleanup-receipt path if it already exists",
            path.display()
        )
    })?;
    sync_parent_directory(path)?;
    Ok(())
}

fn write_receipt(path: &Path, receipt: &mut CleanupReceipt) -> Result<()> {
    prepare_receipt_parent(path)?;
    receipt.touch();
    let json = serde_json::to_vec_pretty(receipt).context("serialize cleanup receipt")?;
    let parent = effective_parent(path);
    let filename = path
        .file_name()
        .context("cleanup receipt path must end with a file name")?
        .to_string_lossy();
    let temporary_path = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4().simple()));

    let replace_result = (|| -> Result<()> {
        write_new_private_file(&temporary_path, &json).with_context(|| {
            format!(
                "write temporary cleanup receipt {}",
                temporary_path.display()
            )
        })?;
        replace_file(&temporary_path, path)?;
        sync_parent_directory(path)?;
        Ok(())
    })();
    if replace_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    replace_result.with_context(|| format!("update cleanup receipt {}", path.display()))?;
    Ok(())
}

fn prepare_receipt_parent(path: &Path) -> Result<()> {
    ensure!(
        path.file_name().is_some(),
        "cleanup receipt path must end with a file name"
    );
    let parent = effective_parent(path);
    fs::create_dir_all(parent)
        .with_context(|| format!("create cleanup receipt directory {}", parent.display()))
}

fn effective_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn write_new_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open new private file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write private file {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush private file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync private file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).with_context(|| {
        format!(
            "atomically replace {} with {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(not(unix))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_file(destination)
            .with_context(|| format!("remove old receipt {}", destination.display()))?;
    }
    fs::rename(source, destination).with_context(|| {
        format!(
            "replace {} with {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = effective_parent(path);
    fs::File::open(parent)
        .with_context(|| format!("open receipt directory {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync receipt directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_url_replaces_only_path_and_preserves_encoded_credentials_and_query() {
        let result = replace_database_path(
            "mysql://root:p%40ss@localhost:3306/admin?ssl-mode=required",
            "codex_range_bench_0123456789abcdef0123456789abcdef",
            "mysql",
        )
        .expect("replace database");
        let parsed = Url::parse(&result).expect("parse result");
        assert_eq!(parsed.username(), "root");
        assert_eq!(parsed.password(), Some("p%40ss"));
        assert_eq!(
            parsed.path(),
            "/codex_range_bench_0123456789abcdef0123456789abcdef"
        );
        assert_eq!(parsed.query(), Some("ssl-mode=required"));
    }

    #[test]
    fn admin_url_rejects_wrong_scheme_missing_database_and_fragment() {
        assert!(validate_admin_url("postgres://localhost/admin", "mysql").is_err());
        assert!(validate_admin_url("mysql://localhost/", "mysql").is_err());
        assert!(validate_admin_url("mysql://localhost/admin#secret", "mysql").is_err());
    }

    #[test]
    fn generated_name_validator_rejects_user_shaped_or_wildcard_names() {
        assert!(
            validate_generated_database_name("codex_range_bench_0123456789abcdef0123456789abcdef")
                .is_ok()
        );
        assert!(validate_generated_database_name("codex_range_bench_%").is_err());
        assert!(validate_generated_database_name("benchmark").is_err());
        assert!(
            validate_generated_database_name("codex_range_bench_0123456789ABCDEF0123456789ABCDEF")
                .is_err()
        );
    }

    #[test]
    fn receipt_path_is_derived_without_replacing_result_extension() {
        assert_eq!(
            derived_receipt_path(Path::new("benchmark-results/run.json")),
            PathBuf::from("benchmark-results/run.json.cleanup.json")
        );
    }

    #[test]
    fn initial_receipt_contains_exact_names_and_no_urls_or_credentials() {
        let name = "codex_range_bench_0123456789abcdef0123456789abcdef".to_owned();
        let receipt =
            CleanupReceipt::new("0123456789abcdef0123456789abcdef".to_owned(), name.clone());
        let json = serde_json::to_string(&receipt).expect("serialize");
        assert_eq!(receipt.mysql.name, name);
        assert!(!receipt.mysql.create_attempted);
        assert!(!receipt.mysql.created_by_this_run);
        assert!(!receipt.mysql.cleanup_authorized);
        assert!(!receipt.postgres.create_attempted);
        assert!(!receipt.postgres.created_by_this_run);
        assert!(!receipt.postgres.cleanup_authorized);
        assert!(!json.contains("mysql://"));
        assert!(!json.contains("postgres://"));
        assert!(!json.contains("password"));
    }

    #[test]
    fn artifact_paths_must_be_fresh_and_resolve_to_different_files() {
        let directory =
            std::env::temp_dir().join(format!("linux-one-click-test-{}", Uuid::new_v4().simple()));
        let output = directory.join("result.json");
        let receipt = directory.join("result.json.cleanup.json");

        prepare_fresh_artifact_paths(&output, &receipt).expect("fresh distinct artifacts");
        assert!(
            prepare_fresh_artifact_paths(&output, &directory.join(".").join("result.json"))
                .is_err(),
            "path aliases must not allow the result to replace the receipt"
        );

        write_new_private_file(&output, b"existing").expect("create existing output");
        assert!(
            prepare_fresh_artifact_paths(&output, &receipt).is_err(),
            "an existing result must not be overwritten"
        );

        fs::remove_dir_all(&directory).expect("remove isolated test directory");
    }

    #[test]
    fn receipt_is_created_exclusively_and_can_be_replaced() {
        let directory =
            std::env::temp_dir().join(format!("linux-one-click-test-{}", Uuid::new_v4().simple()));
        fs::create_dir(&directory).expect("create test directory");
        let path = directory.join("receipt.json");
        let name = "codex_range_bench_0123456789abcdef0123456789abcdef".to_owned();
        let mut receipt = CleanupReceipt::new("0123456789abcdef0123456789abcdef".to_owned(), name);

        write_initial_receipt(&path, &mut receipt).expect("write initial receipt");
        assert!(
            write_initial_receipt(&path, &mut receipt).is_err(),
            "initial receipt must not overwrite another run"
        );
        receipt.mysql.create_attempted = true;
        receipt.mysql.cleanup_authorized = true;
        write_receipt(&path, &mut receipt).expect("replace receipt");

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read receipt")).expect("parse receipt");
        assert_eq!(value["mysql"]["create_attempted"], true);
        assert_eq!(value["mysql"]["cleanup_authorized"], true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("receipt metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(&directory).expect("remove isolated test directory");
    }
}
