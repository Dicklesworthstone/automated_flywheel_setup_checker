//! Automated ACFS installer verification system CLI
//!
//! Output contract: stdout carries only data for the requested `--format`; all logs go to stderr.
//! `--format json` prints exactly one JSON document per command; `--format jsonl` prints one object
//! per line, each with a `kind` discriminator and `schema_version`.
//!
//! Settings are resolved once (defaults < config file < `AFSC_*` env < explicit CLI flags) and
//! every command reads the resolved [`Settings`]. A single [`RunHeader`] describes each run and is
//! shared by the JSONL stream, the JSON document, and the persisted results file.
//!
//! Exit codes follow [`AfscError`]; command handlers never call `std::process::exit`, so the
//! systemd STOPPING notification and container cleanup always run. SIGINT/SIGTERM cancel the run
//! token: in-flight installers are stopped, their containers removed, and the run is persisted
//! with `interrupted: true`.

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use automated_flywheel_setup_checker::config::RunOrder;
use automated_flywheel_setup_checker::reporting::RunInfo;
use automated_flywheel_setup_checker::config::Config;
use automated_flywheel_setup_checker::reporting::{diff_runs, render_diff, render_run, render_timeline, History};

use automated_flywheel_setup_checker::{
    checksums::{
        cross_check, is_acfs_repo, parse_checksums, profile_drift, scan_acfs_repo,
        validate_checksums, validate_url_policy,
    },
    config::{env_map, resolve, CliOverrides, Settings},
    error::{AfscError, Signal},
    logging::LogFormat,
    parser::classify_error,
    reporting::{ResultPersister, RunHeader},
    runner::{
        parse_memory_limit, resolve_spec, ContainerConfig, ContainerManager, ExecutionBackend,
        GlobalDefaults, InstallerSpec, InstallerTest, InstallerTestRunner, PullPolicy,
        RetryConfig, RunnerConfig, TestResult, TestStatus,
    },
    SystemdWatchdog,
};

type CmdResult = std::result::Result<(), AfscError>;

/// Version of the JSON/JSONL output schema. Additive changes only within a major.
const SCHEMA_VERSION: u32 = 1;

/// `--version` line: crate version, git sha (with -dirty), UTC build date, rustc.
const VERSION_LINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("AFSC_GIT_SHA"),
    ", built ",
    env!("AFSC_BUILD_DATE"),
    ", ",
    env!("AFSC_RUSTC"),
    ")"
);

/// Output format for CLI commands
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
    Prometheus,
    /// Markdown tables (status only): for issues, PR comments and the nightly canary
    Markdown,
}

/// Log line format for stderr
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormatArg {
    Text,
    Json,
}

/// Automated ACFS installer verification system
#[derive(Parser)]
#[command(name = "automated_flywheel_setup_checker")]
#[command(about = "Automated ACFS installer verification system")]
#[command(version = VERSION_LINE)]
#[command(after_help = "Exit codes: 0 success; 1 installer failures; 2 usage or configuration \
error (including invalid checksums.yaml); 3 infrastructure error (Docker unreachable); \
4 validation drift (checksum mismatch or unreachable URL); 130/143 interrupted by SIGINT/SIGTERM")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output format
    #[arg(long, global = true, default_value = "human")]
    format: OutputFormat,

    /// Config file path
    #[arg(long, global = true, env = "ACFS_CONFIG")]
    config: Option<PathBuf>,

    /// Verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress progress and per-result lines in human output (summary only)
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Log line format on stderr
    #[arg(long, global = true, default_value = "text")]
    log_format: LogFormatArg,

    /// Enable systemd watchdog integration
    #[arg(long, global = true, env = "ACFS_WATCHDOG")]
    watchdog: bool,

    /// Override [general].acfs_repo
    #[arg(long, global = true)]
    acfs_repo: Option<PathBuf>,

    /// Override [general].data_dir
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Override [docker].image
    #[arg(long, global = true)]
    image: Option<String>,

    /// Allow file:// installer URLs (tests and local fixtures)
    #[arg(long, global = true)]
    allow_file_urls: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run installer checks
    Check {
        /// Specific installers to check (default: all enabled)
        installers: Vec<String>,

        /// Number of parallel checks: an integer or "auto" (default: [execution].parallel)
        #[arg(long)]
        parallel: Option<String>,

        /// Per-installer timeout in seconds (default: [docker].timeout_seconds)
        #[arg(long)]
        timeout: Option<u64>,

        /// Show what would be tested without running
        #[arg(long)]
        dry_run: bool,

        /// Enable auto-remediation on failure
        #[arg(long)]
        remediate: bool,

        /// Stop on first failure (default: [execution].fail_fast)
        #[arg(long)]
        fail_fast: bool,

        /// Run locally instead of in Docker containers
        #[arg(long)]
        local: bool,

        /// Only remove orphaned afsc-managed containers, then exit
        #[arg(long)]
        reap: bool,

        /// Confirm running installers on this host with --local when not attached to a terminal
        #[arg(short = 'y', long)]
        yes: bool,

        /// Run even if another check holds the run lock (results still get distinct files)
        #[arg(long)]
        allow_concurrent: bool,

        /// Rebuild the prepared Docker image before running (pulls the base, no cache)
        #[arg(long)]
        rebuild_base: bool,

        /// Rerun only the installers that failed, timed out or were cancelled in a run
        /// (run id prefix or "last")
        #[arg(long, value_name = "RUN", conflicts_with = "installers")]
        failed_from: Option<String>,
    },

    /// Serve monitoring health and metrics endpoints
    Serve {
        /// Port override for the shared monitoring listener
        #[arg(long)]
        health_port: Option<u16>,

        /// Metrics port override when running in metrics-only mode
        #[arg(long)]
        metrics_port: Option<u16>,
    },

    /// List known installers from checksums.yaml
    List {
        /// Show only installers that would run (excludes [installers.<name>].skip = true)
        #[arg(long)]
        runnable: bool,
    },

    /// Show results of the last run (or a selected run)
    Status {
        /// Show detailed failure information
        #[arg(long)]
        detailed: bool,

        /// List recent runs instead of showing one
        #[arg(long)]
        list: bool,

        /// Select a run by run id prefix (or "last")
        #[arg(long)]
        run: Option<String>,

        /// Timeline of one installer across runs (oldest first), with flakiness assessment
        #[arg(long, value_name = "INSTALLER", conflicts_with_all = ["list", "diff"])]
        history: Option<String>,

        /// Limit --history to the newest N runs
        #[arg(long, value_name = "N", requires = "history")]
        last: Option<usize>,

        /// Installers whose status changed between two runs (run id prefixes or "last")
        #[arg(long, num_args = 2, value_names = ["RUN_A", "RUN_B"], conflicts_with = "list")]
        diff: Vec<String>,
    },

    /// Validate checksums.yaml format
    Validate {
        /// Path to checksums.yaml file
        #[arg(long)]
        path: Option<PathBuf>,

        /// Also check URLs are accessible
        #[arg(long)]
        check_urls: bool,

        /// Also download installer bytes and verify pinned SHA-256 values
        #[arg(long)]
        check_hashes: bool,

        /// Compare ACFS installer call sites with the built-in execution profiles
        #[arg(long)]
        profile: bool,
    },

    /// Diagnose the environment (Docker, image, ACFS repo, dirs, disk, tools, last run)
    Doctor {
        /// Skip the Docker checks (for --local deployments)
        #[arg(long)]
        local: bool,
    },

    /// Send (or re-send) notifications for a persisted run, or flush the daily digest
    Notify {
        /// Notify for the most recent run (what the systemd ExecStopPost hook uses)
        #[arg(long, conflicts_with_all = ["run", "digest"])]
        last_run: bool,

        /// Notify for a run selected by id prefix (or "last")
        #[arg(long, value_name = "RUN", conflicts_with = "digest")]
        run: Option<String>,

        /// Send one summary for every run queued by `notifications.mode = "daily_digest"`
        #[arg(long)]
        digest: bool,
    },

    /// Classify an error message (for testing)
    ClassifyError {
        /// stderr content
        #[arg(long)]
        stderr: String,

        /// Exit code (negative values allowed, e.g. -1 for "unknown")
        #[arg(long, allow_negative_numbers = true)]
        exit_code: i32,

        /// Show which pattern matched and where
        #[arg(long)]
        explain: bool,
    },

    /// Show current configuration
    Config {
        /// Subcommand
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Clone, Subcommand)]
enum ConfigCmd {
    /// Show current configuration
    Show {
        /// Annotate every value with its source (default, file, env, cli)
        #[arg(long)]
        resolved: bool,
    },
    /// Show default configuration
    Default,
    /// Validate configuration file
    Validate {
        /// Treat unknown keys as errors (exit 2)
        #[arg(long)]
        strict: bool,
    },
}

struct CheckOptions {
    installers: Vec<String>,
    parallel: usize,
    timeout: u64,
    dry_run: bool,
    remediate: bool,
    fail_fast: bool,
    local: bool,
    format: OutputFormat,
    quiet: bool,
    retries: u32,
    rebuild_base: bool,
    failed_from: Option<String>,
}

/// Build the CLI override set from parsed flags (only flags actually passed).
fn cli_overrides(cli: &Cli) -> CliOverrides {
    let mut o = CliOverrides {
        image: cli.image.clone(),
        data_dir: cli.data_dir.clone(),
        acfs_repo: cli.acfs_repo.clone(),
        allow_file_urls: if cli.allow_file_urls { Some(true) } else { None },
        ..Default::default()
    };
    if let Commands::Check { parallel, timeout, fail_fast, .. } = &cli.command {
        o.parallel = parallel.clone();
        o.timeout_seconds = *timeout;
        o.fail_fast = if *fail_fast { Some(true) } else { None };
    }
    o
}

fn infra(e: impl std::fmt::Display) -> AfscError {
    AfscError::Infra(e.to_string())
}

/// `--local` executes upstream installer scripts on this host (inside a temporary HOME with a
/// minimal PATH, but with no container isolation). Interactive shells get a warning; scripts and
/// services must opt in with `--yes` or `AFSC_ALLOW_LOCAL=1`.
fn local_consent(yes: bool) -> CmdResult {
    use std::io::IsTerminal;
    tracing::warn!(
        "--local runs upstream installer scripts on THIS host (temporary HOME, no container isolation); prefer the Docker backend"
    );
    let allowed_by_env = std::env::var("AFSC_ALLOW_LOCAL").map(|v| v == "1").unwrap_or(false);
    if yes || allowed_by_env || std::io::stdin().is_terminal() {
        return Ok(());
    }
    Err(AfscError::Usage(
        "--local executes installer scripts on this host; not attached to a terminal, so pass --yes (or set AFSC_ALLOW_LOCAL=1) to confirm"
            .to_string(),
    ))
}

/// Install SIGINT/SIGTERM handlers that cancel the run token. Returns the signal that fired.
fn spawn_signal_handler(token: CancellationToken) -> Arc<Mutex<Option<Signal>>> {
    let which: Arc<Mutex<Option<Signal>>> = Arc::new(Mutex::new(None));
    let seen = which.clone();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Register synchronously: a signal that arrives before the task is first polled must
        // still be caught (tokio installs the OS handler when the stream is created).
        let term = signal(SignalKind::terminate())
            .map_err(|e| tracing::warn!(error = %e, "Cannot install SIGTERM handler"))
            .ok();
        let int = signal(SignalKind::interrupt())
            .map_err(|e| tracing::warn!(error = %e, "Cannot install SIGINT handler"))
            .ok();
        tokio::spawn(async move {
            let wait = |s: Option<tokio::signal::unix::Signal>| async move {
                match s {
                    Some(mut s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = wait(int) => { *seen.lock().unwrap() = Some(Signal::Interrupt); }
                _ = wait(term) => { *seen.lock().unwrap() = Some(Signal::Terminate); }
            }
            tracing::warn!("Signal received; cancelling the run and cleaning up");
            token.cancel();
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        *seen.lock().unwrap() = Some(Signal::Interrupt);
        tracing::warn!("Signal received; cancelling the run and cleaning up");
        token.cancel();
    });
    which
}

#[tokio::main]
async fn main() {
    let code = match real_main().await {
        Ok(()) => 0,
        Err(e) => {
            if !e.is_silent() {
                eprintln!("Error: {e}");
            }
            e.exit_code()
        }
    };
    std::process::exit(code);
}

async fn real_main() -> CmdResult {
    let cli = Cli::parse();

    // Resolve settings first so the configured log level can seed the subscriber.
    let settings = resolve(cli.config.as_deref(), &env_map(), &cli_overrides(&cli))
        .map_err(|e| AfscError::Config(format!("{e:#}")))?;

    let log_format = match cli.log_format {
        LogFormatArg::Text => LogFormat::Text,
        LogFormatArg::Json => LogFormat::Json,
    };
    automated_flywheel_setup_checker::logging::init_with(
        cli.verbose,
        log_format,
        Some(&settings.config.general.log_level),
    );

    for key in &settings.unknown_keys {
        tracing::warn!(key = %key, "Unknown configuration key ignored");
    }

    // Initialize systemd watchdog if enabled
    let watchdog = if cli.watchdog {
        let wd = Arc::new(SystemdWatchdog::new().with_config(&settings.config.watchdog));
        // Start watchdog ping task
        let _watchdog_handle = wd.clone().start();
        Some(wd)
    } else {
        None
    };

    // Notify systemd we're ready to accept requests
    if let Some(ref wd) = watchdog {
        wd.notify_ready();
    }

    let result = run_command(&cli, &settings, watchdog.as_ref()).await;

    // Notify systemd we're stopping
    if let Some(ref wd) = watchdog {
        wd.notify_stopping();
        wd.stop();
    }

    result
}

async fn run_command(
    cli: &Cli,
    settings: &Settings,
    watchdog: Option<&Arc<SystemdWatchdog>>,
) -> CmdResult {
    if matches!(cli.format, OutputFormat::Prometheus | OutputFormat::Markdown)
        && !matches!(&cli.command, Commands::Status { .. })
    {
        return Err(AfscError::Usage(
            "--format prometheus and --format markdown are only supported for the status command".into(),
        ));
    }

    let config = &settings.config;

    match &cli.command {
        Commands::Check { reap: true, .. } => cmd_reap(settings, cli.format).await,

        Commands::Check { installers, dry_run, remediate, local, yes, allow_concurrent, rebuild_base, failed_from, .. } => {
            if let Some(wd) = watchdog {
                wd.notify_status("Running installer checks");
            }
            if *local && !*dry_run {
                local_consent(*yes)?;
            }
            // One check per data dir at a time unless explicitly allowed; dry runs never lock.
            let _run_lock = if *dry_run || *allow_concurrent {
                None
            } else {
                let lock_dir = config.general.data_dir_path().join("locks");
                match automated_flywheel_setup_checker::lock::RunLock::try_acquire(&lock_dir, "run")
                    .map_err(infra)?
                {
                    Ok(lock) => Some(lock),
                    Err(holder) => {
                        return Err(AfscError::Infra(format!(
                            "another check is running (pid {}, since {}); wait for it or pass --allow-concurrent",
                            holder.pid, holder.since
                        )))
                    }
                }
            };
            cmd_check(
                settings,
                CheckOptions {
                    installers: installers.clone(),
                    parallel: config.execution.parallel.resolve(),
                    timeout: config.docker.timeout_seconds,
                    dry_run: *dry_run,
                    remediate: *remediate,
                    fail_fast: config.execution.fail_fast,
                    local: *local,
                    format: cli.format,
                    quiet: cli.quiet,
                    retries: config.execution.retry_transient,
                    rebuild_base: *rebuild_base,
                    failed_from: failed_from.clone(),
                },
            )
            .await
        }

        Commands::Serve { health_port, metrics_port } => {
            if let Some(wd) = watchdog {
                wd.notify_status("Serving monitoring endpoints");
            }
            automated_flywheel_setup_checker::server::serve_monitoring(
                &config.monitoring,
                *health_port,
                *metrics_port,
                config.general.data_dir_path(),
            )
            .await
            .map_err(|e| {
                let text = format!("{e:#}");
                if text.contains("failed to bind") {
                    AfscError::Infra(text)
                } else if text.contains("disabled in config") || text.contains("not an IP address") {
                    AfscError::Config(text)
                } else {
                    AfscError::Other(e)
                }
            })
        }

        Commands::List { runnable } => cmd_list(settings, *runnable, cli.format),

        Commands::Status { detailed, list, run, history, last, diff } => {
            cmd_status(settings, *detailed, *list, run.as_deref(), history.as_deref(), *last, diff, cli.format)
        }

        Commands::Validate { path, check_urls, check_hashes, profile } => {
            cmd_validate(settings, path.clone(), *check_urls, *check_hashes, *profile, cli.format)
                .await
        }

        Commands::ClassifyError { stderr, exit_code, explain } => {
            cmd_classify_error(stderr, *exit_code, *explain, cli.format)
        }

        Commands::Doctor { local } => cmd_doctor(settings, *local, cli.format).await,

        Commands::Notify { last_run, run, digest } => {
            let selector = if *last_run { Some("last".to_string()) } else { run.clone() };
            cmd_notify(settings, selector.as_deref(), *digest, cli.format).await
        }

        Commands::Config { cmd } => cmd_config(cmd.clone(), settings, cli.format),
    }
}

/// Attach the `kind` discriminator and schema version to a serializable value.
fn with_kind<T: serde::Serialize>(
    kind: &str,
    value: &T,
) -> std::result::Result<serde_json::Value, AfscError> {
    let mut v = serde_json::to_value(value).map_err(|e| AfscError::Other(e.into()))?;
    if let serde_json::Value::Object(map) = &mut v {
        map.insert("kind".to_string(), serde_json::Value::String(kind.to_string()));
        map.insert("schema_version".to_string(), serde_json::json!(SCHEMA_VERSION));
    }
    Ok(v)
}

fn to_json(value: &serde_json::Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).unwrap_or_default()
    } else {
        serde_json::to_string(value).unwrap_or_default()
    }
}

/// Print one human-readable error line (category, severity, suggestion) under a failed result.
fn print_error_line(result: &TestResult) {
    if let Some(err) = &result.error {
        println!(
            "    error: {} ({:?}, retryable={}, confidence={:.0}%)",
            err.category,
            err.severity,
            err.retryable,
            err.confidence * 100.0
        );
        if let Some(s) = &err.suggestion {
            println!("    suggestion: {}", s);
        }
    }
}

/// A result that counts as a failure of the run (skips are not failures).
fn is_failure(r: &TestResult) -> bool {
    matches!(r.status, TestStatus::Failed | TestStatus::TimedOut | TestStatus::Cancelled)
}

fn summary_counts(results: &[TestResult]) -> serde_json::Value {
    let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count();
    let passed = results.iter().filter(|r| r.success).count();
    let failed = results.iter().filter(|r| is_failure(r)).count();
    serde_json::json!({
        "total": results.len(),
        "passed": passed,
        "failed": failed,
        "failed_only": count(TestStatus::Failed),
        "timed_out": count(TestStatus::TimedOut),
        "skipped": count(TestStatus::Skipped),
        "cancelled": count(TestStatus::Cancelled),
        "duration_ms": results.iter().map(|r| r.duration_ms).sum::<u64>(),
    })
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

async fn cmd_doctor(settings: &Settings, local: bool, format: OutputFormat) -> CmdResult {
    use automated_flywheel_setup_checker::doctor::{render_human, run_doctor, DoctorOptions};
    let opts = DoctorOptions {
        skip_docker: local,
        unknown_keys: settings.unknown_keys.clone(),
        config_path: settings.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
    };
    let report = run_doctor(&settings.config, &opts).await;
    match format {
        OutputFormat::Human => print!("{}", render_human(&report)),
        OutputFormat::Json => println!("{}", to_json(&with_kind("doctor", &report)?, true)),
        OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            for c in &report.checks {
                println!("{}", to_json(&with_kind("doctor_check", c)?, false));
            }
            let mut summary = serde_json::json!({
                "passed": report.passed, "warnings": report.warnings, "failed": report.failed, "skipped": report.skipped, "ok": report.ok(),
            });
            summary["kind"] = serde_json::json!("doctor_summary");
            summary["schema_version"] = serde_json::json!(SCHEMA_VERSION);
            println!("{}", to_json(&summary, false));
        }
    }
    if report.ok() {
        Ok(())
    } else {
        Err(AfscError::Infra(format!("doctor found {} failing check(s)", report.failed)))
    }
}

/// Execution order for resolved specs: longest-first (historical median duration, unknown first),
/// name, or manifest order from checksums.yaml.
fn order_specs(
    mut specs: Vec<InstallerSpec>,
    order: RunOrder,
    checksums_path: &Path,
    results_dir: &Path,
) -> Vec<InstallerSpec> {
    match order {
        RunOrder::Name => specs.sort_by(|a, b| a.name.cmp(&b.name)),
        RunOrder::Manifest => {
            let manifest = manifest_order(checksums_path);
            let pos = |n: &str| manifest.iter().position(|m| m == n).unwrap_or(usize::MAX);
            specs.sort_by(|a, b| pos(&a.name).cmp(&pos(&b.name)).then_with(|| a.name.cmp(&b.name)));
        }
        RunOrder::LongestFirst => {
            let history = History::load(results_dir).unwrap_or_default();
            // Unknown durations first (could be long), then longest known first; ties by name.
            let key = |n: &str| history.median_duration_ms(n).map(|d| u64::MAX - d).unwrap_or(0);
            specs.sort_by(|a, b| key(&a.name).cmp(&key(&b.name)).then_with(|| a.name.cmp(&b.name)));
        }
    }
    specs
}

/// Installer names in the order they appear in checksums.yaml.
fn manifest_order(checksums_path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(checksums_path) else { return Vec::new() };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { return Vec::new() };
    value
        .get("installers")
        .and_then(|v| v.as_mapping())
        .map(|m| m.keys().filter_map(|k| k.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn sha256_of_file(path: &std::path::Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| hex::encode(Sha256::digest(bytes)))
}

/// Remove orphaned afsc-managed containers (`check --reap`).
async fn cmd_reap(settings: &Settings, format: OutputFormat) -> CmdResult {
    let config = &settings.config;
    let manager = ContainerManager::try_new(ContainerConfig {
        image: config.docker.image.clone(),
        ..Default::default()
    })
    .map_err(infra)?;
    manager.preflight().await.map_err(infra)?;
    let max_age = Duration::from_secs(config.docker.timeout_seconds.saturating_mul(2).max(60));
    let reaped = manager.reap_orphans(max_age).await.map_err(infra)?;
    match format {
        OutputFormat::Human => {
            if reaped.is_empty() {
                println!("No orphaned containers found");
            } else {
                println!("Removed {} orphaned container(s):", reaped.len());
                for c in &reaped {
                    println!("  {} ({})", c.name, c.reason);
                }
            }
        }
        _ => {
            let doc = serde_json::json!({
                "kind": "reap",
                "schema_version": SCHEMA_VERSION,
                "removed": reaped,
            });
            println!("{}", to_json(&doc, matches!(format, OutputFormat::Json)));
        }
    }
    Ok(())
}

async fn cmd_check(settings: &Settings, options: CheckOptions) -> CmdResult {
    let config = &settings.config;
    let command_started_at = chrono::Utc::now();
    let run_id = uuid::Uuid::new_v4().to_string();
    let checksums_path = config.general.acfs_repo.join("checksums.yaml");

    if !checksums_path.exists() {
        return Err(AfscError::Config(format!(
            "checksums.yaml not found at {:?} (set [general].acfs_repo or --acfs-repo)",
            checksums_path
        )));
    }

    let checksums = parse_checksums(&checksums_path)?;

    // --failed-from: rerun only what failed, timed out or was cancelled in an earlier run.
    let requested: Vec<String> = match &options.failed_from {
        Some(run_ref) => {
            let history = History::load(&config.general.results_dir())?;
            let run = history.find(run_ref).ok_or_else(|| {
                AfscError::Usage(format!("no run matches {run_ref:?} (try: status --list)"))
            })?;
            let failed = history.failed_installers(run);
            if failed.is_empty() {
                let short: String = run.run_id().chars().take(8).collect();
                match options.format {
                    OutputFormat::Human => println!("Nothing to rerun: run {short} had no failures"),
                    _ => println!(
                        "{}",
                        to_json(
                            &serde_json::json!({
                                "kind": "check",
                                "schema_version": SCHEMA_VERSION,
                                "status": "nothing_to_rerun",
                                "run_id": run.run_id(),
                            }),
                            matches!(options.format, OutputFormat::Json)
                        )
                    ),
                }
                return Ok(());
            }
            tracing::info!(run_id = %run.run_id(), installers = ?failed, "Rerunning failures from an earlier run");
            failed
        }
        None => options.installers.clone(),
    };

    let mut enabled: Vec<_> = checksums
        .installers
        .iter()
        .filter(|(name, entry)| entry.enabled && (requested.is_empty() || requested.contains(name)))
        .collect();
    enabled.sort_by(|a, b| a.0.cmp(b.0));

    // URL policy: https always; file:// only when allowed; http:// never.
    let policy_errors = validate_url_policy(&checksums, config.general.allow_file_urls);
    let policy_errors: Vec<_> = policy_errors
        .into_iter()
        .filter(|e| {
            let text = e.to_string();
            requested.is_empty() || requested.iter().any(|n| text.contains(n.as_str()))
        })
        .collect();
    if !policy_errors.is_empty() {
        let list: Vec<String> = policy_errors.iter().map(|e| e.to_string()).collect();
        return Err(AfscError::Config(format!(
            "{} installer URL(s) violate the URL policy:\n  {}",
            list.len(),
            list.join("\n  ")
        )));
    }

    let globals = GlobalDefaults { timeout_seconds: options.timeout, retries: options.retries };
    let specs: Vec<InstallerSpec> = enabled
        .iter()
        .map(|(name, entry)| {
            resolve_spec(name.as_str(), entry, config.installers.get(name.as_str()), globals)
        })
        .collect();
    let specs = order_specs(specs, config.execution.order, &checksums_path, &config.general.results_dir());

    let backend_name = if options.local { "local" } else { "docker" };
    let mut header = RunHeader {
        run_id: run_id.clone(),
        started_at: command_started_at,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        backend: backend_name.to_string(),
        image: if options.local { None } else { Some(config.docker.image.clone()) },
        user: if options.local {
            None
        } else if config.docker.run_as_root || !config.docker.prepare {
            Some("root".to_string())
        } else {
            Some("afsc-user".to_string())
        },
        parallel: options.parallel,
        timeout_seconds: options.timeout,
        retries: options.retries,
        fail_fast: options.fail_fast,
        acfs_repo: config.general.acfs_repo.to_string_lossy().to_string(),
        checksums_sha256: sha256_of_file(&checksums_path),
        config_source: settings.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        data_dir: config.general.data_dir_path().to_string_lossy().to_string(),
        installers_requested: requested.clone(),
        installer_count: enabled.len(),
        dry_run: options.dry_run,
        allow_file_urls: config.general.allow_file_urls,
        image_id: None,
        run_as_root: !options.local && (config.docker.run_as_root || !config.docker.prepare),
        deadline_seconds: config.execution.run_deadline_seconds,
        environment: Some(serde_json::json!({
            "host": hostname(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "tool_version": env!("CARGO_PKG_VERSION"),
        })),
    };
    let run_header = with_kind("run", &header)?;

    if options.dry_run {
        match options.format {
            OutputFormat::Human => {
                println!(
                    "Would check {} installer(s) with {} parallel workers (backend: {}{}):",
                    specs.iter().filter(|s| s.skip_reason.is_none()).count(),
                    options.parallel,
                    backend_name,
                    if options.local { String::new() } else { format!(", image {}", config.docker.image) }
                );
                println!(
                    "Defaults: timeout {}s, {} retries (transient failures only), image build timeout {}s",
                    options.timeout, options.retries, config.docker.build_timeout_seconds
                );
                println!();
                println!("  {:<16} {:<5} {:>8} {:>7} {:<7} command", "installer", "sha", "timeout", "retries", "checks");
                for spec in &specs {
                    if let Some(reason) = &spec.skip_reason {
                        println!("  {:<16} (skipped: {})", spec.name, reason);
                        continue;
                    }
                    let mut checks = Vec::new();
                    if spec.expect_binary.is_some() { checks.push("bin"); }
                    if spec.verify_cmd.is_some() { checks.push("verify"); }
                    if spec.version_cmd.is_some() { checks.push("version"); }
                    let overrides = spec.overridden_fields();
                    println!(
                        "  {:<16} {:<5} {:>7}s {:>7} {:<7} {}{}",
                        spec.name,
                        if spec.sha256.is_some() { "yes" } else { "no" },
                        spec.timeout_seconds,
                        spec.retries,
                        checks.join(","),
                        spec.command_line(),
                        if overrides.is_empty() { String::new() } else { format!("  [overrides: {}]", overrides.join(", ")) }
                    );
                    if !spec.env.is_empty() {
                        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        println!("  {:<16} env: {}", "", env.join(" "));
                    }
                }
            }
            OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                let mut output = run_header.clone();
                let installers: Vec<serde_json::Value> = specs
                    .iter()
                    .map(|spec| {
                        let mut v = serde_json::to_value(spec).unwrap_or_default();
                        v["command_line"] = serde_json::json!(spec.command_line());
                        v["overrides"] = serde_json::json!(spec.overridden_fields());
                        v
                    })
                    .collect();
                output["installers"] = serde_json::json!(installers);
                output["build_timeout_seconds"] = serde_json::json!(config.docker.build_timeout_seconds);
                println!("{}", to_json(&output, matches!(options.format, OutputFormat::Json)));
            }
        }
        return Ok(());
    }

    // Docker preflight and orphan reaping before any container work.
    let mut reaped_orphans: Vec<String> = Vec::new();
    if !options.local {
        ContainerManager::preflight_default().await.map_err(infra)?;
        if config.docker.reap_orphans {
            let manager = ContainerManager::try_new(ContainerConfig {
                image: config.docker.image.clone(),
                ..Default::default()
            })
            .map_err(infra)?;
            let max_age = Duration::from_secs(options.timeout.saturating_mul(2).max(60));
            match manager.reap_orphans(max_age).await {
                Ok(reaped) if !reaped.is_empty() => {
                    tracing::warn!(count = reaped.len(), "Removed orphaned containers from earlier runs");
                    reaped_orphans = reaped.iter().map(|c| c.name.clone()).collect();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Orphan reaping failed"),
            }
        }
    }

    // Build the prepared image once, up front (workers would otherwise all wait on the build
    // lock), and record what actually ran in the header.
    if !options.local {
        let manager = ContainerManager::try_new(ContainerConfig {
            image: config.docker.image.clone(),
            prepare: config.docker.prepare,
            build_timeout_seconds: config.docker.build_timeout_seconds,
            rebuild: options.rebuild_base,
            ..Default::default()
        })
        .map_err(infra)?
        .with_pull_policy(PullPolicy::parse_policy(&config.docker.pull_policy));
        manager.ensure_image().await.map_err(infra)?;
        header.image_id = manager.image_id().await;
        if let Ok(v) = manager.docker().version().await {
            if let Some(env) = header.environment.as_mut().and_then(|e| e.as_object_mut()) {
                env.insert("docker_version".into(), serde_json::json!(v.version));
                env.insert("docker_os".into(), serde_json::json!(v.os));
                env.insert("docker_arch".into(), serde_json::json!(v.arch));
                env.insert("docker_kernel".into(), serde_json::json!(v.kernel_version));
            }
        }
    }
    let run_header = with_kind("run", &header)?;

    // Structured event log (audit trail); failures never abort the run.
    let mut event_log = match automated_flywheel_setup_checker::reporting::EventLog::open(
        &config.general.log_dir_path(),
        config.general.log_retention_days,
        &run_id,
    ) {
        Ok(log) => Some(log),
        Err(e) => {
            tracing::warn!(error = %e, "Event log unavailable");
            None
        }
    };
    if let Some(log) = event_log.as_mut() {
        log.event(
            "run_started",
            serde_json::json!({
                "backend": backend_name,
                "image": header.image,
                "parallel": options.parallel,
                "timeout_seconds": options.timeout,
                "retries": options.retries,
                "installer_count": specs.iter().filter(|s| s.skip_reason.is_none()).count(),
                "skipped": specs.iter().filter(|s| s.skip_reason.is_some()).count(),
                "tool_version": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    if let (Some(log), false) = (event_log.as_mut(), reaped_orphans.is_empty()) {
        log.warn_event("reaper", serde_json::json!({ "removed": reaped_orphans }));
    }

    // Cancellation: signals cancel the token; workers stop promptly and clean up.
    let cancel = CancellationToken::new();
    let signal_seen = spawn_signal_handler(cancel.clone());

    // Whole-run deadline (execution.run_deadline_seconds): cancels the token; remaining work is
    // reported as cancelled and the summary carries `deadline_exceeded`.
    let deadline_hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deadline_seconds = config.execution.run_deadline_seconds;
    if deadline_seconds > 0 {
        let token = cancel.clone();
        let flag = deadline_hit.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(deadline_seconds)).await;
            if !token.is_cancelled() {
                tracing::warn!(deadline_seconds, "Run deadline exceeded; cancelling remaining installers");
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                token.cancel();
            }
        });
    }

    // Select execution backend
    let backend = if options.local {
        ExecutionBackend::Local
    } else {
        let container_config = ContainerConfig {
            image: config.docker.image.clone(),
            memory_limit: parse_memory_limit(&config.docker.memory_limit),
            cpu_quota: Some(config.docker.cpu_quota),
            timeout_seconds: options.timeout,
            volumes: config
                .docker
                .volumes
                .iter()
                .filter_map(|spec| {
                    // host:container[:ro] -> (host[:ro], container)
                    let mut parts = spec.splitn(3, ':');
                    let host = parts.next()?.to_string();
                    let container = parts.next()?.to_string();
                    let mode = parts.next().map(|m| format!(":{m}")).unwrap_or_default();
                    Some((format!("{host}{mode}"), container))
                })
                .collect(),
            environment: Vec::new(),
            labels: vec![("afsc.run_id".to_string(), run_id.clone())],
            prepare: config.docker.prepare,
            build_timeout_seconds: config.docker.build_timeout_seconds,
            rebuild: options.rebuild_base,
            network_mode: if config.docker.network.trim().is_empty() || config.docker.network == "bridge" {
                None
            } else {
                Some(config.docker.network.clone())
            },
            run_as_root: config.docker.run_as_root,
        };
        ExecutionBackend::Docker {
            container_config,
            pull_policy: PullPolicy::parse_policy(&config.docker.pull_policy),
        }
    };

    // Set up the runner with configuration
    let runner_config = RunnerConfig {
        default_timeout: Duration::from_secs(options.timeout),
        dry_run: false,
        backend,
        retry: RetryConfig::executor_default(options.retries),
        cancel: cancel.clone(),
        max_capture_bytes: config.execution.max_capture_bytes as usize,
        ..Default::default()
    };
    let runner = InstallerTestRunner::new(runner_config.clone());

    // Skipped installers produce results without running anything.
    let mut skipped_results: Vec<TestResult> = specs
        .iter()
        .filter_map(|spec| {
            spec.skip_reason.as_ref().map(|reason| {
                tracing::info!(installer = %spec.name, reason = %reason, "Skipping installer");
                TestResult::new(&spec.name).skipped(format!("skipped: {reason}"))
            })
        })
        .collect();

    // Runnable specs become executor inputs (interpreter, args, env, checks, limits).
    let tests: Vec<InstallerTest> = specs
        .iter()
        .filter(|spec| spec.skip_reason.is_none() && !spec.url.is_empty())
        .map(|spec| spec.to_test())
        .collect();

    // In JSONL mode the run header is the first line.
    if matches!(options.format, OutputFormat::Jsonl) {
        println!("{}", to_json(&run_header, false));
    }

    // Run tests — use parallel runner when parallel > 1
    let results = if options.parallel > 1 {
        use automated_flywheel_setup_checker::runner::ParallelRunner;
        let pool = ParallelRunner::new(options.parallel, runner_config.clone())
            .with_fail_fast(options.fail_fast);
        pool.run_all(tests).await?
    } else {
        // Sequential execution: same result shape as the pool (queued work after a cancellation
        // is Cancelled, after a fail-fast stop it is Skipped) so every requested installer is
        // accounted for in the summary.
        let mut sequential_results = Vec::new();
        let mut queue = tests.iter();
        for test in queue.by_ref() {
            let result = runner.run_test_with_retry(test).await?;
            let failed = !result.success;
            sequential_results.push(result);
            if cancel.is_cancelled() || (options.fail_fast && failed) {
                break;
            }
        }
        for test in queue {
            let r = if cancel.is_cancelled() {
                let mut r = TestResult::new(&test.name)
                    .cancelled(format!("{} before start", automated_flywheel_setup_checker::parser::CANCELLED_MARKER));
                automated_flywheel_setup_checker::runner::finalize_failure(&mut r, None);
                r
            } else {
                TestResult::new(&test.name).skipped("Skipped due to fail-fast")
            };
            sequential_results.push(r);
        }
        sequential_results
    };

    let mut results = results;
    results.append(&mut skipped_results);
    results.sort_by(|a, b| a.installer_name.cmp(&b.installer_name));

    let deadline_exceeded = deadline_hit.load(std::sync::atomic::Ordering::SeqCst);
    let signalled = signal_seen.lock().unwrap().is_some();
    let interrupted = cancel.is_cancelled();
    // Deadline cancellations are a policy outcome, not an installer failure: exit 1 only when
    // something actually failed or timed out (or a signal interrupted the run).
    let any_failed = results
        .iter()
        .any(|r| is_failure(r) && !(deadline_exceeded && !signalled && r.status == TestStatus::Cancelled));
    if deadline_exceeded && !signalled {
        tracing::warn!(
            cancelled = results.iter().filter(|r| r.status == TestStatus::Cancelled).count(),
            deadline_seconds,
            "Run stopped at the deadline; cancelled installers were not tested"
        );
    }

    if let Some(log) = event_log.as_mut() {
        for r in &results {
            log.installer_event(
                "installer_finished",
                &r.installer_name,
                serde_json::json!({
                    "status": r.status.as_str(),
                    "exit_code": r.exit_code,
                    "duration_ms": r.duration_ms,
                    "attempts": r.attempts.len().max(1),
                    "checksum": r.checksum_state.as_str(),
                    "category": r.error.as_ref().map(|e| e.category.clone()),
                    "installed_version": r.installed_version,
                }),
            );
        }
        let mut summary = summary_counts(&results);
        summary["interrupted"] = serde_json::json!(interrupted);
        log.event("run_finished", summary);
        log.flush();
    }

    // Print per-result output
    for result in &results {
        match options.format {
            OutputFormat::Human => {
                if options.quiet {
                    continue;
                }
                let status_icon = if result.success { "\u{2713}" } else { "\u{2717}" };
                let attempts = if result.attempts.len() > 1 {
                    format!(", {} attempts", result.attempts.len())
                } else {
                    String::new()
                };
                println!(
                    "{} {} ({:?}, {}ms{})",
                    status_icon, result.installer_name, result.status, result.duration_ms, attempts
                );
                if !result.success && !result.stderr.is_empty() {
                    let stderr_preview: String =
                        result.stderr.lines().take(3).collect::<Vec<_>>().join("\n");
                    println!("    stderr: {}", stderr_preview);
                }
                if !result.success {
                    print_error_line(result);
                }
            }
            OutputFormat::Json => {}
            OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                println!("{}", to_json(&with_kind("result", result)?, false));
            }
        }
    }

    // Summary output
    let mut summary = summary_counts(&results);
    summary["run_id"] = serde_json::json!(run_id);
    summary["interrupted"] = serde_json::json!(interrupted);
    summary["deadline_exceeded"] = serde_json::json!(deadline_exceeded);
    let exit_code = match (*signal_seen.lock().unwrap(), any_failed) {
        (Some(Signal::Interrupt), _) => 130,
        (Some(Signal::Terminate), _) => 143,
        (None, true) => 1,
        (None, false) => 0,
    };
    summary["exit_code"] = serde_json::json!(exit_code);
    match options.format {
        OutputFormat::Human => {
            let passed = results.iter().filter(|r| r.success).count();
            let failed = results.iter().filter(|r| is_failure(r)).count();
            let skipped = results.iter().filter(|r| r.status == TestStatus::Skipped).count();
            if !options.quiet {
                println!();
            }
            if deadline_exceeded && !signalled {
                println!(
                    "Run deadline of {deadline_seconds}s exceeded: {} installer(s) cancelled (not tested)",
                    results.iter().filter(|r| r.status == TestStatus::Cancelled).count()
                );
            } else if interrupted {
                println!(
                    "Run interrupted: {} installer(s) cancelled or skipped",
                    results
                        .iter()
                        .filter(|r| matches!(r.status, TestStatus::Cancelled | TestStatus::Skipped))
                        .count()
                );
            }
            println!(
                "Results: {} passed, {} failed{} out of {} total",
                passed,
                failed,
                if skipped > 0 { format!(", {skipped} skipped") } else { String::new() },
                results.len()
            );
        }
        OutputFormat::Json => {
            let output = serde_json::json!({
                "kind": "check",
                "schema_version": SCHEMA_VERSION,
                "run": run_header,
                "results": results,
                "summary": summary,
            });
            println!("{}", to_json(&output, true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            let mut line = summary.clone();
            line["kind"] = serde_json::json!("summary");
            line["schema_version"] = serde_json::json!(SCHEMA_VERSION);
            println!("{}", to_json(&line, false));
        }
    }

    // Remediation for failures (when --remediate is enabled and the run was not interrupted)
    if options.remediate && any_failed && !interrupted {
        use automated_flywheel_setup_checker::remediation::{
            generate_prompt, ClaudeRemediation, ClaudeRemediationConfig as RemConfig,
        };

        if matches!(options.format, OutputFormat::Human) {
            println!("\nAttempting auto-remediation for failures...");
        }

        let rem_config = RemConfig {
            enabled: true,
            cost_limit_usd: config.remediation.cost_limit_usd as f32,
            timeout_seconds: config.remediation.timeout_seconds,
            max_attempts: config.remediation.max_attempts,
            auto_commit: config.remediation.auto_commit,
            create_pr: config.remediation.create_pr,
            ..Default::default()
        };
        let remediation = ClaudeRemediation::new(config.general.acfs_repo.clone(), rem_config);

        for result in results.iter().filter(|r| is_failure(r)) {
            // Classification is always attached to failures; fall back defensively.
            let classification = result.error.clone().unwrap_or_else(|| {
                automated_flywheel_setup_checker::parser::classify_error(
                    &result.stderr,
                    result.exit_code.unwrap_or(-1),
                )
            });
            let prompt =
                generate_prompt(&classification, &result.stderr, &config.general.acfs_repo);

            match remediation.execute_with_resilience(&prompt).await {
                Ok(rem_result) => {
                    if matches!(options.format, OutputFormat::Human) {
                        let status = if rem_result.success { "succeeded" } else { "partial" };
                        println!(
                            "\n  Remediation {} for {} (method: {:?}, cost: ${:.4})",
                            status,
                            result.installer_name,
                            rem_result.method,
                            rem_result.estimated_cost_usd
                        );
                        if !rem_result.changes_made.is_empty() {
                            println!("  Files to modify:");
                            for change in &rem_result.changes_made {
                                println!(
                                    "    - {} ({:?})",
                                    change.path.display(),
                                    change.change_type
                                );
                            }
                        }
                        if !rem_result.claude_output.is_empty() {
                            let preview: String = rem_result
                                .claude_output
                                .lines()
                                .take(5)
                                .collect::<Vec<_>>()
                                .join("\n    ");
                            println!("  Output: {}", preview);
                        }
                    }
                }
                Err(e) => {
                    if matches!(options.format, OutputFormat::Human) {
                        println!("  Remediation failed for {}: {}", result.installer_name, e);
                    }
                }
            }
        }
    }

    // Persist results to JSONL file under the data dir
    let persister = ResultPersister::new(config.general.results_dir());
    match persister.persist_with_header(&results, &header, interrupted) {
        Ok(path) => {
            if matches!(options.format, OutputFormat::Human) && !options.quiet {
                println!("Results saved to: {}", path.display());
            }
            tracing::info!(path = %path.display(), "Results saved");
            match persister.prune(config.general.results_retention as usize) {
                Ok(0) => {}
                Ok(n) => tracing::info!(pruned = n, "Old results pruned"),
                Err(e) => tracing::warn!(error = %e, "Failed to prune old results"),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to persist results");
        }
    }

    let started_at = results.first().map(|r| r.started_at).unwrap_or(command_started_at);
    match persist_metrics_snapshot(
        &config.general.data_dir_path(),
        config.monitoring.stale_after_seconds,
        options.remediate && any_failed,
        started_at,
    ) {
        Ok(path) => {
            tracing::debug!(path = %path.display(), "Metrics snapshot updated");
        }
        Err(error) => {
            tracing::warn!(error = %error, "Failed to persist metrics snapshot");
        }
    }

    // Notifications work from the persisted run so `notify --last-run` reproduces them exactly.
    if config.notifications.enabled && !signalled {
        let history = History::load(&config.general.results_dir()).unwrap_or_default();
        if let Some(run) = history.find(&run_id) {
            let outcome = dispatch_notification(config, &history, run, false).await;
            if let Some(log) = event_log.as_mut() {
                log.event("notification", outcome);
                log.flush();
            }
        }
    }

    if let Some(sig) = *signal_seen.lock().unwrap() {
        return Err(AfscError::Interrupted(sig));
    }
    if any_failed {
        return Err(AfscError::InstallerFailures {
            failed: results.iter().filter(|r| is_failure(r)).count(),
            total: results.len(),
        });
    }

    Ok(())
}

fn cmd_list(settings: &Settings, runnable: bool, format: OutputFormat) -> CmdResult {
    let config = &settings.config;
    let checksums_path = config.general.acfs_repo.join("checksums.yaml");

    if !checksums_path.exists() {
        return Err(AfscError::Config(format!("checksums.yaml not found at {:?}", checksums_path)));
    }

    let checksums = parse_checksums(&checksums_path)?;
    let globals = GlobalDefaults {
        timeout_seconds: config.docker.timeout_seconds,
        retries: config.execution.retry_transient,
    };

    let mut rows: Vec<(String, &automated_flywheel_setup_checker::checksums::InstallerEntry, InstallerSpec)> =
        checksums
            .installers
            .iter()
            .map(|(name, entry)| {
                let spec = resolve_spec(name, entry, config.installers.get(name), globals);
                (name.clone(), entry, spec)
            })
            .filter(|(_, entry, spec)| !runnable || (entry.enabled && spec.skip_reason.is_none()))
            .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    match format {
        OutputFormat::Human => {
            println!("Installers ({}):", rows.len());
            for (name, entry, spec) in &rows {
                let has_checksum = if entry.sha256.is_some() { " sha256" } else { "" };
                let skip = spec
                    .skip_reason
                    .as_ref()
                    .map(|r| format!(" [skip: {r}]"))
                    .unwrap_or_default();
                let overrides = spec.overridden_fields();
                let ov = if overrides.is_empty() {
                    String::new()
                } else {
                    format!(" [overrides: {}]", overrides.join(", "))
                };
                println!("  {} - {}{}{}{}", name, spec.command_line(), has_checksum, skip, ov);
            }
        }
        OutputFormat::Json => {
            let output: Vec<_> = rows
                .iter()
                .map(|(name, entry, spec)| {
                    serde_json::json!({
                        "name": name,
                        "url": entry.url,
                        "sha256": entry.sha256,
                        "enabled": entry.enabled,
                        "interpreter": spec.interpreter,
                        "args": spec.args,
                        "skip_reason": spec.skip_reason,
                        "overrides": spec.overridden_fields(),
                    })
                })
                .collect();
            println!("{}", to_json(&serde_json::Value::Array(output), true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            for (name, entry, spec) in &rows {
                let output = serde_json::json!({
                    "kind": "installer",
                    "schema_version": SCHEMA_VERSION,
                    "name": name,
                    "url": entry.url,
                    "sha256": entry.sha256,
                    "enabled": entry.enabled,
                    "interpreter": spec.interpreter,
                    "args": spec.args,
                    "skip_reason": spec.skip_reason,
                    "overrides": spec.overridden_fields(),
                });
                println!("{}", to_json(&output, false));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_status(
    settings: &Settings,
    detailed: bool,
    list: bool,
    run: Option<&str>,
    history_of: Option<&str>,
    last: Option<usize>,
    diff: &[String],
    format: OutputFormat,
) -> CmdResult {
    let config = &settings.config;

    if let Some(installer) = history_of {
        return cmd_status_history(config, installer, last, format);
    }
    if let [a, b] = diff {
        return cmd_status_diff(config, a, b, format);
    }

    if matches!(format, OutputFormat::Prometheus) {
        let report = automated_flywheel_setup_checker::reporting::MetricsReport::from_data_dir(
            &config.general.data_dir_path(),
            chrono::Utc::now(),
            config.monitoring.stale_after_seconds,
        )?;
        print!("{}", report.to_prometheus());
        return Ok(());
    }

    let persister = ResultPersister::new(config.general.results_dir());

    if list {
        let runs = persister.list_runs()?;
        match format {
            OutputFormat::Human => {
                if runs.is_empty() {
                    println!("No runs found. Run: automated_flywheel_setup_checker check");
                    return Ok(());
                }
                println!("Recent runs ({}), newest first:", runs.len());
                println!("  {:<8}  {:<20}  {:>5}  {:>6}  {:>6}  note", "run", "started (UTC)", "total", "passed", "failed");
                for r in &runs {
                    println!(
                        "  {:<8}  {:<20}  {:>5}  {:>6}  {:>6}  {}",
                        r.run_id.chars().take(8).collect::<String>(),
                        r.started_at.format("%Y-%m-%d %H:%M:%S"),
                        r.total,
                        r.passed,
                        r.failed,
                        if r.interrupted { "interrupted" } else { "" }
                    );
                }
            }
            OutputFormat::Json => {
                let output = serde_json::json!({
                    "kind": "runs",
                    "schema_version": SCHEMA_VERSION,
                    "runs": runs,
                });
                println!("{}", to_json(&output, true));
            }
            OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                for r in &runs {
                    println!("{}", to_json(&with_kind("run_info", r)?, false));
                }
            }
        }
        return Ok(());
    }

    let selected = match run {
        Some(prefix) => match persister.find_run(prefix)? {
            Some(info) => Some(info.path),
            None => {
                return Err(AfscError::Usage(format!(
                    "no run matches {:?} (try: status --list)",
                    prefix
                )))
            }
        },
        None => persister.latest_results()?,
    };

    let results_path = match selected {
        Some(path) => path,
        None => {
            match format {
                OutputFormat::Human => {
                    println!("No runs found. Run: automated_flywheel_setup_checker check");
                }
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                    let output = serde_json::json!({
                        "kind": "status",
                        "schema_version": SCHEMA_VERSION,
                        "status": "no_runs",
                        "message": "No runs recorded yet"
                    });
                    println!("{}", to_json(&output, matches!(format, OutputFormat::Json)));
                }
            }
            return Ok(());
        }
    };

    let file = ResultPersister::read_run_file(&results_path)?;
    let entries = &file.entries;
    let summary = &file.summary;

    // Flakiness / breakage labels need more than one run; cheap to compute from headers + entries.
    let history = History::load(&config.general.results_dir()).unwrap_or_default();
    let assessments: std::collections::BTreeMap<String, automated_flywheel_setup_checker::reporting::Assessment> =
        if history.len() > 1 {
            entries.iter().map(|e| (e.installer_name.clone(), history.assess(&e.installer_name))).collect()
        } else {
            Default::default()
        };

    if matches!(format, OutputFormat::Markdown) {
        let run_id = file.header.as_ref().map(|h| h.run_id.clone())
            .or_else(|| summary.as_ref().map(|s| s.run_id.clone()))
            .unwrap_or_default();
        let loaded = history.find(&run_id).map(|r| render_run(r, &assessments));
        match loaded {
            Some(md) => print!("{md}"),
            None => {
                let run = automated_flywheel_setup_checker::reporting::LoadedRun {
                    info: RunInfo { path: results_path.clone(), run_id, started_at: chrono::Utc::now(), total: entries.len(), passed: 0, failed: 0, interrupted: false },
                    header: file.header.clone(),
                    entries: Vec::new(),
                    summary: file.summary.clone(),
                };
                print!("{}", render_run(&run, &assessments));
            }
        }
        return Ok(());
    }

    match format {
        OutputFormat::Human => {
            if let Some(ref s) = summary {
                println!(
                    "Last run: {} ({} total, {} passed, {} failed, {} skipped{})",
                    s.run_id.chars().take(8).collect::<String>(),
                    s.total,
                    s.passed,
                    s.failed,
                    s.skipped,
                    if s.interrupted { ", interrupted" } else { "" }
                );
                println!("Duration: {}ms", s.duration_total_ms);
                println!(
                    "Time: {} - {}",
                    s.timestamp_start.format("%Y-%m-%d %H:%M:%S"),
                    s.timestamp_end.format("%H:%M:%S")
                );
                if let Some(h) = &file.header {
                    println!(
                        "Backend: {}{}  parallel={}  timeout={}s  retries={}",
                        h.backend,
                        h.image.as_ref().map(|i| format!(" ({i})")).unwrap_or_default(),
                        h.parallel,
                        h.timeout_seconds,
                        h.retries
                    );
                }
                println!();
            }

            for entry in entries {
                let icon = match entry.status.as_str() {
                    "passed" => "\u{2713}",
                    "failed" => "\u{2717}",
                    "timedout" => "\u{29D6}",
                    "cancelled" => "\u{2298}",
                    "skipped" => "-",
                    _ => "?",
                };
                let checksum = if entry.sha256_verified { " sha256" } else { "" };
                let label = assessments
                    .get(&entry.installer_name)
                    .and_then(|a| a.label())
                    .map(|l| format!("  [{l}]"))
                    .unwrap_or_default();
                println!(
                    "  {} {} ({}ms{}){}{}",
                    icon,
                    entry.installer_name,
                    entry.duration_ms,
                    if entry.retry_count > 0 {
                        format!(", {} retries", entry.retry_count)
                    } else {
                        String::new()
                    },
                    checksum,
                    label
                );

                if detailed && !entry.stderr_excerpt.is_empty() && entry.status != "passed" {
                    let preview: String =
                        entry.stderr_excerpt.lines().take(3).collect::<Vec<_>>().join("\n      ");
                    println!("      stderr: {}", preview);
                }
                if detailed {
                    if let Some(ref ec) = entry.error_classification {
                        println!(
                            "      error: {} ({}, retryable={}, confidence={:.0}%)",
                            ec.category,
                            ec.severity,
                            ec.retryable,
                            ec.confidence * 100.0
                        );
                    }
                    if !entry.checksum_state.is_empty() {
                        println!("      checksum: {}", entry.checksum_state);
                    }
                    for attempt in &entry.attempts {
                        println!(
                            "      attempt {}: {} exit={} {}ms waited={}ms",
                            attempt.index,
                            attempt.status,
                            attempt.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                            attempt.duration_ms,
                            attempt.waited_before_ms
                        );
                    }
                }
            }

            println!("\nResults file: {}", results_path.display());
        }
        OutputFormat::Json => {
            let output = serde_json::json!({
                "kind": "status",
                "schema_version": SCHEMA_VERSION,
                "run": file.header,
                "results": entries,
                "summary": summary,
                "assessments": assessments,
                "file": results_path.to_string_lossy(),
            });
            println!("{}", to_json(&output, true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            if let Some(h) = &file.header {
                println!("{}", to_json(&with_kind("run", h)?, false));
            }
            for entry in entries {
                println!("{}", to_json(&with_kind("result", entry)?, false));
            }
            if let Some(s) = summary {
                println!("{}", to_json(&with_kind("summary", s)?, false));
            }
        }
    }

    Ok(())
}

fn cmd_status_history(
    config: &Config,
    installer: &str,
    last: Option<usize>,
    format: OutputFormat,
) -> CmdResult {
    let history = History::load(&config.general.results_dir())?;
    let mut entries = history.installer_timeline(installer);
    if entries.is_empty() {
        return Err(AfscError::Usage(format!(
            "no history for installer {installer:?} (known: {})",
            history.installers().into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(n) = last {
        let skip = entries.len().saturating_sub(n);
        entries.drain(..skip);
    }
    let assessment = history.assess(installer);
    match format {
        OutputFormat::Human => {
            println!(
                "{installer}: {} run(s){}",
                entries.len(),
                assessment.label().map(|l| format!("  [{l}]")).unwrap_or_default()
            );
            println!(
                "  {:<8}  {:<16}  {:<9}  {:<11}  {:>8}  {:>4}  {:<8}  version",
                "run", "started (UTC)", "status", "category", "duration", "att", "script"
            );
            for e in &entries {
                println!(
                    "  {:<8}  {:<16}  {:<9}  {:<11}  {:>7}ms  {:>4}  {:<8}  {}",
                    e.run_id.chars().take(8).collect::<String>(),
                    e.started_at.format("%Y-%m-%d %H:%M"),
                    e.status,
                    e.category.as_deref().unwrap_or(""),
                    e.duration_ms,
                    e.attempts,
                    e.script_sha256.as_deref().map(|s| s.chars().take(8).collect::<String>()).unwrap_or_default(),
                    e.installed_version.as_deref().unwrap_or("")
                );
            }
            println!(
                "  pass probability {:.0}% over {} trial(s) since the last script change ({} script version(s) seen)",
                assessment.pass_probability * 100.0,
                assessment.trials,
                assessment.script_versions
            );
        }
        OutputFormat::Json => {
            let output = serde_json::json!({
                "kind": "history",
                "schema_version": SCHEMA_VERSION,
                "installer": installer,
                "entries": entries,
                "assessment": assessment,
            });
            println!("{}", to_json(&output, true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            for e in &entries {
                let mut line = with_kind("history_entry", e)?;
                line["installer"] = serde_json::json!(installer);
                println!("{}", to_json(&line, false));
            }
            let mut line = with_kind("assessment", &assessment)?;
            line["installer"] = serde_json::json!(installer);
            println!("{}", to_json(&line, false));
        }
        OutputFormat::Markdown => print!("{}", render_timeline(installer, &entries, &assessment)),
    }
    Ok(())
}

fn cmd_status_diff(config: &Config, a: &str, b: &str, format: OutputFormat) -> CmdResult {
    let history = History::load(&config.general.results_dir())?;
    let find = |prefix: &str| {
        history
            .find(prefix)
            .ok_or_else(|| AfscError::Usage(format!("no run matches {prefix:?} (try: status --list)")))
    };
    let (from, to) = (find(a)?, find(b)?);
    let diff = diff_runs(from, to);
    match format {
        OutputFormat::Human => {
            println!(
                "Diff {} -> {}: {} changed, {} unchanged",
                diff.from_run.chars().take(8).collect::<String>(),
                diff.to_run.chars().take(8).collect::<String>(),
                diff.changes.len(),
                diff.unchanged
            );
            for c in &diff.changes {
                println!(
                    "  {:<10} {:<16} {} -> {}",
                    c.change,
                    c.installer,
                    c.before.as_deref().unwrap_or("-"),
                    c.after.as_deref().unwrap_or("-")
                );
            }
        }
        OutputFormat::Json => {
            println!("{}", to_json(&with_kind("diff", &diff)?, true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            for c in &diff.changes {
                let mut line = with_kind("diff_entry", c)?;
                line["from_run"] = serde_json::json!(diff.from_run);
                line["to_run"] = serde_json::json!(diff.to_run);
                println!("{}", to_json(&line, false));
            }
        }
        OutputFormat::Markdown => print!("{}", render_diff(&diff)),
    }
    Ok(())
}

async fn cmd_validate(
    settings: &Settings,
    path: Option<PathBuf>,
    check_urls_flag: bool,
    check_hashes_flag: bool,
    profile_flag: bool,
    format: OutputFormat,
) -> CmdResult {
    use automated_flywheel_setup_checker::checksums::{check_hashes, check_urls};

    let config = &settings.config;
    let checksums_path = path.unwrap_or_else(|| config.general.acfs_repo.join("checksums.yaml"));

    if !checksums_path.exists() {
        return Err(AfscError::Config(format!("checksums.yaml not found at {:?}", checksums_path)));
    }

    let checksums = parse_checksums(&checksums_path)?;
    let mut result = validate_checksums(&checksums, false); // format validation only
    for error in validate_url_policy(&checksums, config.general.allow_file_urls) {
        result.add_error(error);
    }

    // Cross-check against the ACFS repository when it is available next to checksums.yaml.
    let repo_root = checksums_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut cross = None;
    let mut drift = Vec::new();
    if is_acfs_repo(&repo_root) {
        match scan_acfs_repo(&repo_root) {
            Ok(scan) => {
                let cc = cross_check(&checksums, &scan.known_installers);
                for name in &cc.missing_from_checksums {
                    result.add_error(automated_flywheel_setup_checker::checksums::ValidationError::MissingUrl(
                        format!("{name} (in KNOWN_INSTALLERS but not in checksums.yaml)"),
                    ));
                }
                for (name, yaml_url, known_url) in &cc.url_mismatches {
                    result.add_error(automated_flywheel_setup_checker::checksums::ValidationError::InvalidUrl(
                        name.clone(),
                        format!("checksums.yaml has {yaml_url} but KNOWN_INSTALLERS has {known_url}"),
                    ));
                }
                for name in &cc.extra_in_checksums {
                    result.add_warning(format!("{name} is in checksums.yaml but not in KNOWN_INSTALLERS"));
                }
                if profile_flag {
                    drift = profile_drift(&scan.call_sites);
                    for d in &drift {
                        result.add_warning(format!(
                            "profile drift for {}: ACFS {} = {:?} but built-in profile has {:?} ({}:{})",
                            d.name, d.field, d.acfs, d.profile, d.file, d.line
                        ));
                    }
                }
                cross = Some(cc);
            }
            Err(e) => result.add_warning(format!("ACFS repo scan failed: {e:#}")),
        }
    } else if profile_flag {
        result.add_warning(format!(
            "--profile: no ACFS checkout found at {} (scripts/lib/security.sh missing)",
            repo_root.display()
        ));
    }

    let format_report = serde_json::json!({
        "valid": result.valid,
        "errors": result.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
        "warnings": result.warnings,
    });

    if matches!(format, OutputFormat::Human) {
        if result.valid {
            println!("checksums.yaml is valid");
        } else {
            println!("checksums.yaml has errors:");
            for error in &result.errors {
                println!("  ERROR: {}", error);
            }
        }
        if !result.warnings.is_empty() {
            println!("Warnings:");
            for warning in &result.warnings {
                println!("  WARN: {}", warning);
            }
        }
    }

    // Accumulate one document for json; stream kinds for jsonl.
    let mut report = serde_json::json!({
        "kind": "validate",
        "schema_version": SCHEMA_VERSION,
        "path": checksums_path.to_string_lossy(),
        "format": format_report,
        "cross_check": cross,
        "profile_drift": drift,
    });
    // 2 = format errors, 4 = drift or unreachable URLs, 0 = clean
    let mut exit_code = if result.valid { 0 } else { 2 };
    let mut drift_summary = Vec::new();

    if matches!(format, OutputFormat::Jsonl) {
        let mut line = serde_json::json!({
            "kind": "format",
            "schema_version": SCHEMA_VERSION,
            "path": checksums_path.to_string_lossy(),
        });
        line["valid"] = report["format"]["valid"].clone();
        line["errors"] = report["format"]["errors"].clone();
        line["warnings"] = report["format"]["warnings"].clone();
        println!("{}", to_json(&line, false));
    }

    // URL checking (async) — only when the format is valid
    if result.valid && check_urls_flag {
        if matches!(format, OutputFormat::Human) {
            println!();
            println!("Checking URLs...");
        }
        let url_results = check_urls(&checksums).await;

        let reachable = url_results.iter().filter(|r| r.reachable).count();
        let broken = url_results.len() - reachable;

        match format {
            OutputFormat::Human => {
                for r in &url_results {
                    let icon = if r.reachable { "\u{2713}" } else { "\u{2717}" };
                    let status_str = r
                        .status
                        .map(|s| format!("HTTP {}", s))
                        .unwrap_or_else(|| "error".to_string());
                    let error_str =
                        r.error.as_ref().map(|e| format!(" ({})", e)).unwrap_or_default();
                    println!(
                        "  {} {} - {} {}ms{}",
                        icon, r.name, status_str, r.response_time_ms, error_str
                    );
                }
                println!();
                println!(
                    "URL check: {} reachable, {} broken out of {} total",
                    reachable,
                    broken,
                    url_results.len()
                );
            }
            OutputFormat::Json => {
                report["url_checks"] = serde_json::json!({
                    "results": url_results,
                    "summary": { "total": url_results.len(), "reachable": reachable, "broken": broken },
                });
            }
            OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                for r in &url_results {
                    println!("{}", to_json(&with_kind("url_check", r)?, false));
                }
            }
        }

        if broken > 0 {
            exit_code = 4;
            drift_summary.push(format!("{broken} unreachable URL(s)"));
        }
    }

    // Hash checking (async)
    if result.valid && check_hashes_flag {
        if matches!(format, OutputFormat::Human) {
            println!();
            println!("Checking hashes...");
        }
        let hash_results = check_hashes(&checksums).await;

        let matched = hash_results.iter().filter(|r| r.matches).count();
        let mismatched = hash_results.len() - matched;

        match format {
            OutputFormat::Human => {
                for r in &hash_results {
                    let icon = if r.matches { "\u{2713}" } else { "\u{2717}" };
                    let error_str =
                        r.error.as_ref().map(|e| format!(" ({})", e)).unwrap_or_default();
                    println!("  {} {} - {}ms{}", icon, r.name, r.response_time_ms, error_str);
                    if !r.matches {
                        if let Some(expected) = &r.expected {
                            println!("      expected: {}", expected);
                        }
                        if let Some(actual) = &r.actual {
                            println!("      actual:   {}", actual);
                        }
                    }
                }
                println!();
                println!(
                    "Hash check: {} matched, {} mismatched out of {} total",
                    matched,
                    mismatched,
                    hash_results.len()
                );
            }
            OutputFormat::Json => {
                report["hash_checks"] = serde_json::json!({
                    "results": hash_results,
                    "summary": { "total": hash_results.len(), "matched": matched, "mismatched": mismatched },
                });
            }
            OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                for r in &hash_results {
                    println!("{}", to_json(&with_kind("hash_check", r)?, false));
                }
            }
        }

        if mismatched > 0 {
            exit_code = 4;
            drift_summary.push(format!("{mismatched} checksum mismatch(es)"));
        }

        // Persist for metrics (`afsc_checksum_drift_total`) and doctor.
        let validation = automated_flywheel_setup_checker::reporting::ValidationReport {
            checked_at: Some(chrono::Utc::now()),
            checksums_path: checksums_path.to_string_lossy().to_string(),
            total: hash_results.len() as u64,
            matched: matched as u64,
            mismatched: hash_results.iter().filter(|r| !r.matches && r.error.is_none()).map(|r| r.name.clone()).collect(),
            unreachable: hash_results.iter().filter(|r| !r.matches && r.error.is_some()).map(|r| r.name.clone()).collect(),
        };
        if let Err(e) = validation.save(&config.general.data_dir_path()) {
            tracing::warn!(error = %e, "Failed to persist validation report");
        }
    }

    report["exit_code"] = serde_json::json!(exit_code);
    match format {
        OutputFormat::Json => println!("{}", to_json(&report, true)),
        OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            let line = serde_json::json!({
                "kind": "summary",
                "schema_version": SCHEMA_VERSION,
                "valid": result.valid,
                "exit_code": exit_code,
            });
            println!("{}", to_json(&line, false));
        }
        OutputFormat::Human => {}
    }

    match exit_code {
        0 => Ok(()),
        2 => Err(AfscError::ChecksumsInvalid(format!(
            "checksums.yaml has {} error(s)",
            result.errors.len()
        ))),
        _ => Err(AfscError::ValidationDrift(drift_summary.join(", "))),
    }
}

fn cmd_classify_error(
    stderr: &str,
    exit_code: i32,
    explain: bool,
    format: OutputFormat,
) -> CmdResult {
    let classification = classify_error(stderr, exit_code);
    let explanation = if explain {
        automated_flywheel_setup_checker::parser::explain(stderr, exit_code)
    } else {
        None
    };

    match format {
        OutputFormat::Human => {
            println!("Error Classification:");
            println!("  Severity: {:?}", classification.severity);
            println!("  Category: {}", classification.category);
            println!("  Retryable: {}", classification.retryable);
            println!("  Confidence: {:.0}%", classification.confidence * 100.0);
            if let Some(suggestion) = &classification.suggestion {
                println!("  Suggestion: {}", suggestion);
            }
            if explain {
                match &explanation {
                    Some((cat, pattern, offset)) => {
                        println!("  Matched: {} via `{}` at byte {}", cat, pattern, offset)
                    }
                    None => println!("  Matched: no pattern (fallback classification)"),
                }
            }
        }
        OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
            let mut v = with_kind("classification", &classification)?;
            if explain {
                v["explain"] = match &explanation {
                    Some((cat, pattern, offset)) => serde_json::json!({
                        "category": cat, "pattern": pattern, "offset": offset
                    }),
                    None => serde_json::Value::Null,
                };
            }
            println!("{}", to_json(&v, matches!(format, OutputFormat::Json)));
        }
    }

    Ok(())
}

fn cmd_config(cmd: ConfigCmd, settings: &Settings, format: OutputFormat) -> CmdResult {
    match cmd {
        ConfigCmd::Show { resolved } => {
            if resolved {
                match format {
                    OutputFormat::Human => {
                        println!("# Resolved configuration (value  # source)");
                        print!("{}", settings.render_annotated()?);
                    }
                    OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                        let output = serde_json::json!({
                            "kind": "config",
                            "schema_version": SCHEMA_VERSION,
                            "config": settings.config,
                            "sources": settings.sources,
                            "unknown_keys": settings.unknown_keys,
                            "config_path": settings.config_path,
                        });
                        println!("{}", to_json(&output, true));
                    }
                }
                return Ok(());
            }
            match format {
                OutputFormat::Human => {
                    println!("Current configuration:");
                    println!(
                        "{}",
                        toml::to_string_pretty(&settings.config)
                            .map_err(|e| AfscError::Other(e.into()))?
                    );
                }
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&settings.config)
                            .map_err(|e| AfscError::Other(e.into()))?
                    );
                }
            }
        }
        ConfigCmd::Default => {
            let config = automated_flywheel_setup_checker::Config::default();
            match format {
                OutputFormat::Human => {
                    println!("Default configuration:");
                    println!(
                        "{}",
                        toml::to_string_pretty(&config).map_err(|e| AfscError::Other(e.into()))?
                    );
                }
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus | OutputFormat::Markdown => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&config)
                            .map_err(|e| AfscError::Other(e.into()))?
                    );
                }
            }
        }
        ConfigCmd::Validate { strict } => match &settings.config_path {
            Some(path) => {
                // Settings already resolved successfully (a parse error would have exited earlier).
                for key in &settings.unknown_keys {
                    println!("WARN: unknown configuration key: {}", key);
                }
                if strict && !settings.unknown_keys.is_empty() {
                    return Err(AfscError::Config(format!(
                        "Configuration file has {} unknown key(s): {:?}",
                        settings.unknown_keys.len(),
                        path
                    )));
                }
                println!("Configuration file is valid: {:?}", path);
            }
            None => {
                println!("No configuration file specified, using defaults");
            }
        },
    }

    Ok(())
}

fn persist_metrics_snapshot(
    data_dir: &std::path::Path,
    stale_after_seconds: u64,
    remediation_attempted: bool,
    started_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<std::path::PathBuf> {
    use automated_flywheel_setup_checker::reporting::{MetricsReport, MetricsSnapshot, WINDOW};
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("metrics.json");
    let now = chrono::Utc::now();

    // Remediation attempts are the one thing not derivable from result files: carry them over
    // while the previous snapshot is inside the window.
    let previous = MetricsSnapshot::load(&path).ok().filter(|s| now - s.snapshot_time <= WINDOW);
    let mut remediations = previous.map(|s| s.total_remediations_24h).unwrap_or(0);
    if remediation_attempted {
        remediations += 1;
    }

    let report = MetricsReport::from_data_dir(data_dir, now, stale_after_seconds)?;
    let uptime_seconds = (now - started_at).num_seconds().max(0) as u64;
    report.snapshot(remediations, uptime_seconds).save(&path)?;
    Ok(path)
}

/// Build the notification for a persisted run: Markdown body (GitHub) plus structured
/// failures and summary fields (Slack). Captured hints are redacted.
fn build_notification(
    run: &automated_flywheel_setup_checker::reporting::LoadedRun,
    assessments: &std::collections::BTreeMap<String, automated_flywheel_setup_checker::reporting::Assessment>,
    kind: &str,
) -> automated_flywheel_setup_checker::reporting::Notification {
    use automated_flywheel_setup_checker::reporting::{is_failure_status, redact, FailureLine, Notification};

    let short: String = run.run_id().chars().take(8).collect();
    let total = run.entries.len();
    let passed = run.entries.iter().filter(|e| e.status == "passed").count();
    let skipped = run.entries.iter().filter(|e| e.status == "skipped").count();
    let failing: Vec<_> = run.entries.iter().filter(|e| is_failure_status(&e.status)).collect();
    let title = match kind {
        "recovered" => format!("AFSC: all {total} installers passing again (run {short})"),
        "failure" => format!("AFSC: {} of {total} installers failing (run {short})", failing.len()),
        _ => format!("AFSC: {passed}/{total} passed (run {short})"),
    };
    let mut body = render_run(run, assessments);
    body.push_str(&format!("\nRun id: `{}`  \nResults file: `{}`\n", run.run_id(), run.info.path.display()));
    let failures = failing
        .iter()
        .map(|e| FailureLine {
            installer: e.installer_name.clone(),
            status: e.status.clone(),
            category: e.error_classification.as_ref().map(|c| c.category.clone()).unwrap_or_else(|| "unknown".into()),
            severity: e.error_classification.as_ref().map(|c| c.severity.clone()).unwrap_or_default(),
            duration_ms: e.duration_ms,
            attempts: e.attempts.len().max(1),
            hint: redact(e.stderr_tail.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")),
        })
        .collect();
    let mut summary_fields = vec![
        ("Passed".to_string(), passed.to_string()),
        ("Failed".to_string(), failing.len().to_string()),
        ("Skipped".to_string(), skipped.to_string()),
        ("Started (UTC)".to_string(), run.started_at().format("%Y-%m-%d %H:%M").to_string()),
    ];
    if let Some(h) = &run.header {
        summary_fields.push(("Backend".to_string(), format!("{}{}", h.backend, h.image.as_ref().map(|i| format!(" ({i})")).unwrap_or_default())));
    }
    Notification {
        title,
        body_markdown: body,
        is_failure: !failing.is_empty(),
        run_id: run.run_id().to_string(),
        summary_fields,
        failures,
        kind: kind.to_string(),
    }
}

fn pending_digest_path(config: &Config) -> PathBuf {
    config.general.data_dir_path().join("notify").join("pending.jsonl")
}

/// Decide per `notifications.mode` and send. `force` bypasses the mode (explicit `notify`).
/// Returns a JSON document describing what happened (also written to the event log).
async fn dispatch_notification(
    config: &Config,
    history: &History,
    run: &automated_flywheel_setup_checker::reporting::LoadedRun,
    force: bool,
) -> serde_json::Value {
    use automated_flywheel_setup_checker::config::NotificationMode;
    use automated_flywheel_setup_checker::reporting::Notifier;

    let failing_now = run.failing_set();
    let previous = history.previous(run.run_id());
    let previously_failing = previous.map(|p| p.failing_set()).unwrap_or_default();
    let kind = if failing_now.is_empty() {
        if previously_failing.is_empty() { "success" } else { "recovered" }
    } else {
        "failure"
    };
    let mode = config.notifications.mode;
    let mode_name = format!("{mode:?}").to_lowercase();

    let decision = if force {
        "send"
    } else {
        match mode {
            NotificationMode::EveryRun => "send",
            NotificationMode::OnChange => {
                if previous.is_some() && previously_failing == failing_now {
                    "unchanged"
                } else {
                    "send"
                }
            }
            NotificationMode::DailyDigest => {
                let path = pending_digest_path(config);
                let line = serde_json::json!({
                    "run_id": run.run_id(),
                    "started_at": run.started_at(),
                    "total": run.entries.len(),
                    "failing": failing_now,
                    "path": run.info.path.to_string_lossy(),
                });
                let queued = path
                    .parent()
                    .map(std::fs::create_dir_all)
                    .and_then(|r| r.ok())
                    .and_then(|_| {
                        use std::io::Write;
                        std::fs::OpenOptions::new().create(true).append(true).open(&path).ok().and_then(|mut f| writeln!(f, "{line}").ok())
                    })
                    .is_some();
                if queued { "queued" } else { "queue_failed" }
            }
        }
    };

    if decision != "send" {
        tracing::info!(mode = %mode_name, decision, kind, "Notification not sent");
        return serde_json::json!({ "mode": mode_name, "decision": decision, "kind": kind, "run_id": run.run_id() });
    }

    let assessments: std::collections::BTreeMap<_, _> = if history.len() > 1 {
        run.entries.iter().map(|e| (e.installer_name.clone(), history.assess(&e.installer_name))).collect()
    } else {
        Default::default()
    };
    let notification = build_notification(run, &assessments, kind);
    let notifier = Notifier::new(config.notifications.to_internal());
    match notifier.send(&notification).await {
        Ok(outcome) => {
            tracing::info!(kind, github = ?outcome.github, slack = ?outcome.slack, "Notification sent");
            serde_json::json!({
                "mode": mode_name,
                "decision": "sent",
                "kind": kind,
                "run_id": run.run_id(),
                "title": notification.title,
                "github": outcome.github,
                "github_issue": outcome.github_issue,
                "slack": outcome.slack,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "Notification delivery failed");
            serde_json::json!({ "mode": mode_name, "decision": "failed", "kind": kind, "run_id": run.run_id(), "error": e.to_string() })
        }
    }
}

/// `notify --last-run | --run <id> | --digest`.
async fn cmd_notify(settings: &Settings, selector: Option<&str>, digest: bool, format: OutputFormat) -> CmdResult {
    use automated_flywheel_setup_checker::reporting::{Notification, Notifier};

    let config = &settings.config;
    if !config.notifications.enabled {
        return Err(AfscError::Config(
            "notifications are disabled: set [notifications].enabled = true (or AFSC_NOTIFICATIONS_ENABLED=1)".into(),
        ));
    }
    let history = History::load(&config.general.results_dir())?;

    let outcome = if digest {
        let path = pending_digest_path(config);
        let pending: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if pending.is_empty() {
            serde_json::json!({ "kind": "notify", "schema_version": SCHEMA_VERSION, "decision": "nothing_pending", "runs": 0 })
        } else {
            let last = &pending[pending.len() - 1];
            let last_run = last["run_id"].as_str().and_then(|id| history.find(id));
            let failing: std::collections::BTreeMap<String, String> = last["failing"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("unknown").to_string())).collect())
                .unwrap_or_default();
            let title = format!("AFSC daily digest: {} run(s), {} installer(s) currently failing", pending.len(), failing.len());
            let mut body = format!("## AFSC daily digest — {} run(s)\n\n| run | started (UTC) | total | failing |\n|---|---|---|---|\n", pending.len());
            for p in &pending {
                let started = p["started_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                let names: Vec<&str> = p["failing"].as_object().map(|m| m.keys().map(String::as_str).collect()).unwrap_or_default();
                body.push_str(&format!("| `{}` | {} | {} | {} |\n", p["run_id"].as_str().unwrap_or("").chars().take(8).collect::<String>(), started, p["total"], if names.is_empty() { "—".to_string() } else { names.join(", ") }));
            }
            let mut notification = match last_run {
                Some(run) => {
                    let mut n = build_notification(run, &Default::default(), if failing.is_empty() { "success" } else { "failure" });
                    body.push_str("\n### Latest run\n\n");
                    body.push_str(&n.body_markdown);
                    n.body_markdown = body;
                    n
                }
                None => Notification {
                    title: title.clone(),
                    body_markdown: body,
                    is_failure: !failing.is_empty(),
                    run_id: last["run_id"].as_str().unwrap_or("").to_string(),
                    summary_fields: Vec::new(),
                    failures: Vec::new(),
                    kind: "digest".into(),
                },
            };
            notification.title = title;
            notification.kind = "digest".into();
            let notifier = Notifier::new(config.notifications.to_internal());
            let sent = notifier.send(&notification).await.map_err(AfscError::Other)?;
            // Keep the record: rotate the queue instead of deleting it.
            let rotated = path.with_file_name(format!("sent_{}.jsonl", chrono::Utc::now().format("%Y%m%dT%H%M%S")));
            if let Err(e) = std::fs::rename(&path, &rotated) {
                tracing::warn!(error = %e, "Failed to rotate the digest queue");
            }
            serde_json::json!({
                "kind": "notify", "schema_version": SCHEMA_VERSION, "decision": "sent", "digest": true,
                "runs": pending.len(), "title": notification.title,
                "github": sent.github, "github_issue": sent.github_issue, "slack": sent.slack,
                "queue": rotated.to_string_lossy(),
            })
        }
    } else {
        let selector = selector.unwrap_or("last");
        let run = history
            .find(selector)
            .ok_or_else(|| AfscError::Usage(format!("no run matches {selector:?} (try: status --list)")))?;
        let mut doc = dispatch_notification(config, &history, run, true).await;
        doc["kind"] = serde_json::json!("notify");
        doc["schema_version"] = serde_json::json!(SCHEMA_VERSION);
        doc
    };

    match format {
        OutputFormat::Human => {
            println!(
                "Notification: {}{}{}{}",
                outcome["decision"].as_str().unwrap_or("?"),
                outcome["title"].as_str().map(|t| format!(" — {t}")).unwrap_or_default(),
                outcome["github"].as_str().map(|g| format!(" [github: {g}{}]", outcome["github_issue"].as_u64().map(|n| format!(" #{n}")).unwrap_or_default())).unwrap_or_default(),
                outcome["slack"].as_str().map(|s| format!(" [slack: {s}]")).unwrap_or_default(),
            );
        }
        _ => println!("{}", to_json(&outcome, matches!(format, OutputFormat::Json))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_kind_adds_discriminator_and_schema_version() {
        let v = with_kind("result", &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(v["kind"], "result");
        assert_eq!(v["schema_version"], SCHEMA_VERSION);
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn summary_counts_statuses() {
        let results = vec![
            TestResult::new("a").passed(),
            TestResult::new("b").failed(1, "x"),
            TestResult::new("c").timed_out(),
            TestResult::new("d").skipped("s"),
            TestResult::new("e").cancelled("c"),
        ];
        let s = summary_counts(&results);
        assert_eq!(s["total"], 5);
        assert_eq!(s["passed"], 1);
        assert_eq!(s["failed"], 3, "skips are not failures");
        assert_eq!(s["timed_out"], 1);
        assert_eq!(s["skipped"], 1);
        assert_eq!(s["cancelled"], 1);
    }

    #[test]
    fn cli_overrides_only_carry_passed_flags() {
        let cli = Cli::try_parse_from(["afsc", "check", "--parallel", "auto", "--fail-fast"]).unwrap();
        let o = cli_overrides(&cli);
        assert_eq!(o.parallel.as_deref(), Some("auto"));
        assert_eq!(o.fail_fast, Some(true));
        assert!(o.timeout_seconds.is_none());
        assert!(o.image.is_none());

        let cli = Cli::try_parse_from(["afsc", "--image", "ubuntu:24.04", "status"]).unwrap();
        let o = cli_overrides(&cli);
        assert_eq!(o.image.as_deref(), Some("ubuntu:24.04"));
        assert!(o.fail_fast.is_none());
    }

    /// Every ExecStart/ExecStopPost in the shipped unit templates must parse with this CLI.
    #[test]
    fn systemd_unit_templates_parse_with_this_cli() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("systemd");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "in") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let rendered = text
                .replace("@BIN@", "afsc")
                .replace("@USER@", "svc")
                .replace("@DATA_DIR@", "/var/lib/flywheel-checker")
                .replace("@LOG_DIR@", "/var/log/flywheel-checker")
                .replace("@CONFIG_DIR@", "/etc/flywheel-checker")
                .replace("@CONFIG@", "/etc/flywheel-checker/config.toml");
            assert!(!rendered.contains('@'), "unrendered placeholder in {}", path.display());
            assert!(!rendered.contains("NOTIFY_SOCKET"), "{}: never pin NOTIFY_SOCKET", path.display());
            assert!(!rendered.contains("IOReadBandwidthMax"), "{}: no device pins", path.display());
            for line in rendered.lines() {
                let Some(cmd) = line.strip_prefix("ExecStart=").or_else(|| line.strip_prefix("ExecStopPost=")) else { continue };
                let argv: Vec<&str> = cmd.split_whitespace().collect();
                assert_eq!(argv[0], "afsc", "{}: {line}", path.display());
                Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{}: {line}\n{e}", path.display()));
                checked += 1;
            }
            if path.file_name().is_some_and(|n| n != "automated-flywheel-checker-serve.service.in") {
                assert!(rendered.contains("ReadWritePaths=/var/lib/flywheel-checker /var/log/flywheel-checker"), "{}", path.display());
                assert!(rendered.contains("SupplementaryGroups=docker"), "{}", path.display());
                assert!(rendered.contains("RestrictAddressFamilies=AF_UNIX"), "{}: docker socket needs AF_UNIX", path.display());
            }
        }
        assert!(checked >= 5, "expected ExecStart lines in the templates, checked {checked}");
    }

    #[test]
    fn systemd_style_command_lines_parse() {
        // Guards the unit templates: these are the ExecStart shapes C13 will ship.
        for line in [
            "afsc --config /etc/flywheel-checker/config.toml --format json --watchdog check",
            "afsc --config /etc/flywheel-checker/config.toml --format json --watchdog check --parallel 4",
            "afsc --config /etc/flywheel-checker/config.toml serve",
            "afsc check --reap",
        ] {
            let argv: Vec<&str> = line.split_whitespace().collect();
            assert!(Cli::try_parse_from(argv).is_ok(), "should parse: {line}");
        }
        assert!(Cli::try_parse_from(["afsc", "check", "--all", "--json"]).is_err());
    }
}
