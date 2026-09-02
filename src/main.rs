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

/// Output format for CLI commands
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
    Prometheus,
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
#[command(version)]
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
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "Cannot install SIGTERM handler");
                    let _ = tokio::signal::ctrl_c().await;
                    *seen.lock().unwrap() = Some(Signal::Interrupt);
                    token.cancel();
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { *seen.lock().unwrap() = Some(Signal::Interrupt); }
                _ = term.recv() => { *seen.lock().unwrap() = Some(Signal::Terminate); }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            *seen.lock().unwrap() = Some(Signal::Interrupt);
        }
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
    if matches!(cli.format, OutputFormat::Prometheus)
        && !matches!(&cli.command, Commands::Status { .. })
    {
        return Err(AfscError::Usage(
            "--format prometheus is only supported for the status command".into(),
        ));
    }

    let config = &settings.config;

    match &cli.command {
        Commands::Check { reap: true, .. } => cmd_reap(settings, cli.format).await,

        Commands::Check { installers, dry_run, remediate, local, yes, allow_concurrent, rebuild_base, .. } => {
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
                config.general.metrics_path(),
            )
            .await
            .map_err(|e| {
                let text = format!("{e:#}");
                if text.contains("failed to bind") {
                    AfscError::Infra(text)
                } else if text.contains("disabled in config") {
                    AfscError::Config(text)
                } else {
                    AfscError::Other(e)
                }
            })
        }

        Commands::List { runnable } => cmd_list(settings, *runnable, cli.format),

        Commands::Status { detailed, list, run } => {
            cmd_status(settings, *detailed, *list, run.as_deref(), cli.format)
        }

        Commands::Validate { path, check_urls, check_hashes, profile } => {
            cmd_validate(settings, path.clone(), *check_urls, *check_hashes, *profile, cli.format)
                .await
        }

        Commands::ClassifyError { stderr, exit_code, explain } => {
            cmd_classify_error(stderr, *exit_code, *explain, cli.format)
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
    let mut enabled: Vec<_> = checksums
        .installers
        .iter()
        .filter(|(name, entry)| {
            entry.enabled && (options.installers.is_empty() || options.installers.contains(name))
        })
        .collect();
    enabled.sort_by(|a, b| a.0.cmp(b.0));

    // URL policy: https always; file:// only when allowed; http:// never.
    let policy_errors = validate_url_policy(&checksums, config.general.allow_file_urls);
    let policy_errors: Vec<_> = policy_errors
        .into_iter()
        .filter(|e| {
            let text = e.to_string();
            options.installers.is_empty() || options.installers.iter().any(|n| text.contains(n.as_str()))
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

    let backend_name = if options.local { "local" } else { "docker" };
    let header = RunHeader {
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
        installers_requested: options.installers.clone(),
        installer_count: enabled.len(),
        dry_run: options.dry_run,
        allow_file_urls: config.general.allow_file_urls,
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
            OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Orphan reaping failed"),
            }
        }
    }

    // Cancellation: signals cancel the token; workers stop promptly and clean up.
    let cancel = CancellationToken::new();
    let signal_seen = spawn_signal_handler(cancel.clone());

    // Select execution backend
    let backend = if options.local {
        ExecutionBackend::Local
    } else {
        let container_config = ContainerConfig {
            image: config.docker.image.clone(),
            memory_limit: parse_memory_limit(&config.docker.memory_limit),
            cpu_quota: Some(config.docker.cpu_quota),
            timeout_seconds: options.timeout,
            volumes: Vec::new(),
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
        // Sequential execution
        let mut sequential_results = Vec::new();
        for test in &tests {
            let result = runner.run_test_with_retry(test).await?;
            let failed = !result.success;
            sequential_results.push(result);
            if cancel.is_cancelled() {
                break;
            }
            if options.fail_fast && failed {
                break;
            }
        }
        sequential_results
    };

    let mut results = results;
    results.append(&mut skipped_results);
    results.sort_by(|a, b| a.installer_name.cmp(&b.installer_name));

    let interrupted = cancel.is_cancelled();
    let any_failed = results.iter().any(is_failure);

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
            OutputFormat::Jsonl | OutputFormat::Prometheus => {
                println!("{}", to_json(&with_kind("result", result)?, false));
            }
        }
    }

    // Summary output
    let mut summary = summary_counts(&results);
    summary["run_id"] = serde_json::json!(run_id);
    summary["interrupted"] = serde_json::json!(interrupted);
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
            if interrupted {
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
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
        &config.general.metrics_path(),
        &results,
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

    if config.notifications.enabled && !interrupted {
        let notifier = automated_flywheel_setup_checker::reporting::Notifier::new(
            config.notifications.to_internal(),
        );
        let (title, body) = build_notification_summary(&results, &run_id, started_at);
        if let Err(error) = notifier.notify(&title, &body, any_failed).await {
            tracing::warn!(error = %error, "Notification delivery failed");
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
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
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

fn cmd_status(
    settings: &Settings,
    detailed: bool,
    list: bool,
    run: Option<&str>,
    format: OutputFormat,
) -> CmdResult {
    use automated_flywheel_setup_checker::reporting::{MetricsExporter, MetricsSnapshot};

    let config = &settings.config;

    if matches!(format, OutputFormat::Prometheus) {
        let mut snapshot = MetricsSnapshot::load_or_default(&config.general.metrics_path());
        snapshot.reset_if_stale();
        let exporter = MetricsExporter::from_snapshot("afsc", &snapshot);
        print!("{}", exporter.export());
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
            OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                println!(
                    "  {} {} ({}ms{}){}",
                    icon,
                    entry.installer_name,
                    entry.duration_ms,
                    if entry.retry_count > 0 {
                        format!(", {} retries", entry.retry_count)
                    } else {
                        String::new()
                    },
                    checksum
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
                "file": results_path.to_string_lossy(),
            });
            println!("{}", to_json(&output, true));
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
            OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
            OutputFormat::Jsonl | OutputFormat::Prometheus => {
                for r in &hash_results {
                    println!("{}", to_json(&with_kind("hash_check", r)?, false));
                }
            }
        }

        if mismatched > 0 {
            exit_code = 4;
            drift_summary.push(format!("{mismatched} checksum mismatch(es)"));
        }
    }

    report["exit_code"] = serde_json::json!(exit_code);
    match format {
        OutputFormat::Json => println!("{}", to_json(&report, true)),
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
        OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                    OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
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
    path: &std::path::Path,
    results: &[automated_flywheel_setup_checker::runner::TestResult],
    remediation_attempted: bool,
    started_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut snapshot =
        automated_flywheel_setup_checker::reporting::MetricsSnapshot::load_or_default(path);
    snapshot.reset_if_stale();

    for result in results {
        snapshot.record_test(result.success);
    }

    if remediation_attempted {
        snapshot.record_remediation();
    }

    let uptime_seconds = (chrono::Utc::now() - started_at).num_seconds().max(0) as u64;
    snapshot.set_uptime(uptime_seconds);
    snapshot.save(path)?;

    Ok(path.to_path_buf())
}

fn build_notification_summary(
    results: &[automated_flywheel_setup_checker::runner::TestResult],
    run_id: &str,
    started_at: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let passed = results.iter().filter(|result| result.success).count();
    let failed = results.iter().filter(|result| !result.success).count();
    let total = results.len();

    let title = if failed > 0 {
        format!("AFSC: {failed} failures in {total} tests")
    } else {
        format!("AFSC: {passed}/{total} passed")
    };

    let mut body = format!(
        "Run ID: {run_id}\nStarted: {}\nPassed: {passed}\nFailed: {failed}\nTotal: {total}",
        started_at.to_rfc3339()
    );

    let failures: Vec<String> = results
        .iter()
        .filter(|result| !result.success)
        .take(5)
        .map(|result| {
            let category =
                result.error.as_ref().map(|error| error.category.as_str()).unwrap_or("unknown");
            format!("- {} ({category})", result.installer_name)
        })
        .collect();

    if !failures.is_empty() {
        body.push_str("\n\nFailures:\n");
        body.push_str(&failures.join("\n"));
    }

    (title, body)
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
