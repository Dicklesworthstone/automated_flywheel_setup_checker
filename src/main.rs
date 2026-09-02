//! Automated ACFS installer verification system CLI
//!
//! Output contract: stdout carries only data for the requested `--format`; all logs go to stderr.
//! `--format json` prints exactly one JSON document per command; `--format jsonl` prints one object
//! per line, each with a `kind` discriminator and `schema_version`.

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

use automated_flywheel_setup_checker::{
    checksums::{parse_checksums, validate_checksums},
    config::load_config,
    logging::LogFormat,
    parser::classify_error,
    runner::{
        ContainerConfig, ExecutionBackend, InstallerTest, InstallerTestRunner, PullPolicy,
        RetryConfig, RunnerConfig, TestResult, TestStatus,
    },
    SystemdWatchdog,
};

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
}

#[derive(Subcommand)]
enum Commands {
    /// Run installer checks
    Check {
        /// Specific installers to check (default: all enabled)
        installers: Vec<String>,

        /// Number of parallel checks (default: [execution].parallel from config)
        #[arg(long)]
        parallel: Option<usize>,

        /// Per-installer timeout in seconds (default: [docker].timeout_seconds from config)
        #[arg(long)]
        timeout: Option<u64>,

        /// Show what would be tested without running
        #[arg(long)]
        dry_run: bool,

        /// Enable auto-remediation on failure
        #[arg(long)]
        remediate: bool,

        /// Stop on first failure (default: [execution].fail_fast from config)
        #[arg(long)]
        fail_fast: bool,

        /// Run locally instead of in Docker containers
        #[arg(long)]
        local: bool,
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
        /// Show only enabled installers
        #[arg(long)]
        enabled_only: bool,

        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },

    /// Show last run results
    Status {
        /// Show detailed failure information
        #[arg(long)]
        detailed: bool,
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
    Show,
    /// Show default configuration
    Default,
    /// Validate configuration file
    Validate,
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
}

const HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS: u64 = 900;

fn base_image_timeout_seconds(requested: u64) -> u64 {
    requested.max(HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS)
}

fn installer_timeout_seconds(installer_name: &str, requested: u64) -> u64 {
    match installer_name {
        "mdwb" => requested.max(HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS),
        _ => requested,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load configuration first so the configured log level can seed the subscriber.
    let config = load_config(cli.config.as_deref())?;

    let log_format = match cli.log_format {
        LogFormatArg::Text => LogFormat::Text,
        LogFormatArg::Json => LogFormat::Json,
    };
    automated_flywheel_setup_checker::logging::init_with(
        cli.verbose,
        log_format,
        Some(&config.general.log_level),
    );

    // Initialize systemd watchdog if enabled
    let watchdog = if cli.watchdog {
        let wd = Arc::new(SystemdWatchdog::new().with_config(&config.watchdog));
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

    let result = run_command(&cli, &config, watchdog.as_ref()).await;

    // Notify systemd we're stopping
    if let Some(ref wd) = watchdog {
        wd.notify_stopping();
        wd.stop();
    }

    result
}

async fn run_command(
    cli: &Cli,
    config: &automated_flywheel_setup_checker::Config,
    watchdog: Option<&Arc<SystemdWatchdog>>,
) -> Result<()> {
    if matches!(cli.format, OutputFormat::Prometheus)
        && !matches!(&cli.command, Commands::Status { .. })
    {
        anyhow::bail!("--format prometheus is only supported for the status command");
    }

    match &cli.command {
        Commands::Check { installers, parallel, timeout, dry_run, remediate, fail_fast, local } => {
            if let Some(wd) = watchdog {
                wd.notify_status("Running installer checks");
            }
            cmd_check(
                config,
                CheckOptions {
                    installers: installers.clone(),
                    parallel: parallel.unwrap_or(config.execution.parallel).max(1),
                    timeout: timeout.unwrap_or(config.docker.timeout_seconds),
                    dry_run: *dry_run,
                    remediate: *remediate,
                    fail_fast: *fail_fast || config.execution.fail_fast,
                    local: *local,
                    format: cli.format,
                    quiet: cli.quiet,
                    retries: config.execution.retry_transient,
                },
            )
            .await?;
        }

        Commands::Serve { health_port, metrics_port } => {
            cmd_serve(&config.monitoring, *health_port, *metrics_port, watchdog).await?;
        }

        Commands::List { enabled_only, tag } => {
            cmd_list(config, *enabled_only, tag.clone(), cli.format)?;
        }

        Commands::Status { detailed } => {
            cmd_status(*detailed, cli.format)?;
        }

        Commands::Validate { path, check_urls, check_hashes } => {
            cmd_validate(config, path.clone(), *check_urls, *check_hashes, cli.format).await?;
        }

        Commands::ClassifyError { stderr, exit_code, explain } => {
            cmd_classify_error(stderr, *exit_code, *explain, cli.format)?;
        }

        Commands::Config { cmd } => {
            cmd_config(cmd.clone(), &cli.config, cli.format)?;
        }
    }

    Ok(())
}

async fn cmd_serve(
    monitoring: &automated_flywheel_setup_checker::config::MonitoringConfig,
    health_port: Option<u16>,
    metrics_port: Option<u16>,
    watchdog: Option<&Arc<SystemdWatchdog>>,
) -> Result<()> {
    if let Some(wd) = watchdog {
        wd.notify_status("Serving monitoring endpoints");
    }

    automated_flywheel_setup_checker::server::serve_monitoring(
        monitoring,
        health_port,
        metrics_port,
    )
    .await
}

/// Attach the `kind` discriminator and schema version to a serializable value.
fn with_kind<T: serde::Serialize>(kind: &str, value: &T) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(value)?;
    if let serde_json::Value::Object(map) = &mut v {
        map.insert("kind".to_string(), serde_json::Value::String(kind.to_string()));
        map.insert("schema_version".to_string(), serde_json::json!(SCHEMA_VERSION));
    }
    Ok(v)
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

fn summary_counts(results: &[TestResult]) -> serde_json::Value {
    let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count();
    let passed = results.iter().filter(|r| r.success).count();
    let failed = results.iter().filter(|r| !r.success).count();
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

async fn cmd_check(
    config: &automated_flywheel_setup_checker::Config,
    options: CheckOptions,
) -> Result<()> {
    use std::time::Duration;

    let command_started_at = chrono::Utc::now();
    let run_id = uuid::Uuid::new_v4().to_string();
    let checksums_path = config.general.acfs_repo.join("checksums.yaml");

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
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

    let backend_name = if options.local { "local" } else { "docker" };
    let run_header = serde_json::json!({
        "kind": "run",
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "started_at": command_started_at,
        "tool_version": env!("CARGO_PKG_VERSION"),
        "backend": backend_name,
        "image": if options.local { serde_json::Value::Null } else { serde_json::json!(config.docker.image) },
        "parallel": options.parallel,
        "timeout_seconds": options.timeout,
        "retries": options.retries,
        "fail_fast": options.fail_fast,
        "acfs_repo": config.general.acfs_repo,
        "installers_requested": options.installers,
        "installer_count": enabled.len(),
        "dry_run": options.dry_run,
    });

    if options.dry_run {
        match options.format {
            OutputFormat::Human => {
                println!(
                    "Would check {} installer(s) with {} parallel workers:",
                    enabled.len(),
                    options.parallel
                );
                println!("Timeout: {}s per installer", options.timeout);
                println!("Retries: {} (transient failures only)", options.retries);
                println!(
                    "Heavy setup floor: {}s for Docker base image and known slow installers",
                    HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS
                );
                println!("Backend: {}", backend_name);
                println!();
                for (name, entry) in &enabled {
                    if let Some(ver) = &entry.version {
                        println!("  - {} (v{})", name, ver);
                    } else {
                        println!("  - {}", name);
                    }
                }
            }
            OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
                let mut output = run_header.clone();
                output["installers"] =
                    serde_json::json!(enabled.iter().map(|(n, _)| n).collect::<Vec<_>>());
                output["heavy_setup_timeout_floor"] = serde_json::json!(HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS);
                if matches!(options.format, OutputFormat::Json) {
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    println!("{}", serde_json::to_string(&output)?);
                }
            }
        }
        return Ok(());
    }

    // Select execution backend
    let backend = if options.local {
        ExecutionBackend::Local
    } else {
        let container_config = ContainerConfig {
            image: config.docker.image.clone(),
            memory_limit: parse_memory_limit(&config.docker.memory_limit),
            cpu_quota: Some(config.docker.cpu_quota),
            timeout_seconds: base_image_timeout_seconds(options.timeout),
            volumes: Vec::new(),
            environment: Vec::new(),
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
        ..Default::default()
    };
    let runner = InstallerTestRunner::new(runner_config.clone());

    // Convert checksums entries to InstallerTest objects
    let max_attempts = options.retries.saturating_add(1).max(1);
    let tests: Vec<InstallerTest> = enabled
        .iter()
        .filter_map(|(name, entry)| {
            // Skip entries without URLs
            let url = entry.url.as_ref()?;
            let timeout = installer_timeout_seconds(name.as_str(), options.timeout);
            let mut test = InstallerTest::new(name.as_str(), url)
                .with_timeout(Duration::from_secs(timeout))
                .with_retry_count(max_attempts);

            // Add checksum if available
            if let Some(sha256) = &entry.sha256 {
                test = test.with_sha256(sha256);
            }

            Some(test)
        })
        .collect();

    // In JSONL mode the run header is the first line.
    if matches!(options.format, OutputFormat::Jsonl) {
        println!("{}", serde_json::to_string(&run_header)?);
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
            sequential_results.push(result);
            if options.fail_fast && sequential_results.last().map(|r| !r.success).unwrap_or(false) {
                break;
            }
        }
        sequential_results
    };

    let any_failed = results.iter().any(|r| !r.success);

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
                println!("{}", serde_json::to_string(&with_kind("result", result)?)?);
            }
        }
    }

    // Summary output
    let mut summary = summary_counts(&results);
    summary["run_id"] = serde_json::json!(run_id);
    summary["exit_code"] = serde_json::json!(if any_failed { 1 } else { 0 });
    match options.format {
        OutputFormat::Human => {
            let passed = results.iter().filter(|r| r.success).count();
            let failed = results.len() - passed;
            if !options.quiet {
                println!();
            }
            println!(
                "Results: {} passed, {} failed out of {} total",
                passed,
                failed,
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
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            let mut line = summary.clone();
            line["kind"] = serde_json::json!("summary");
            line["schema_version"] = serde_json::json!(SCHEMA_VERSION);
            println!("{}", serde_json::to_string(&line)?);
        }
    }

    // Remediation for failures (when --remediate is enabled)
    if options.remediate && any_failed {
        use automated_flywheel_setup_checker::remediation::{
            generate_prompt, ClaudeRemediation, ClaudeRemediationConfig as RemConfig,
        };

        if matches!(options.format, OutputFormat::Human) {
            println!("\nAttempting auto-remediation for failures...");
        }

        let rem_config = RemConfig {
            enabled: true,
            cost_limit_usd: 1.0,
            timeout_seconds: 120,
            ..Default::default()
        };
        let remediation = ClaudeRemediation::new(config.general.acfs_repo.clone(), rem_config);

        for result in results.iter().filter(|r| !r.success) {
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

    // Persist results to JSONL file
    let started_at = results.first().map(|r| r.started_at).unwrap_or(command_started_at);
    let persister = automated_flywheel_setup_checker::reporting::ResultPersister::default_dir();
    match persister.persist(&results, &run_id, started_at) {
        Ok(path) => {
            if matches!(options.format, OutputFormat::Human) && !options.quiet {
                println!("Results saved to: {}", path.display());
            }
            tracing::info!(path = %path.display(), "Results saved");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to persist results");
        }
    }

    match persist_metrics_snapshot(&results, options.remediate && any_failed, started_at) {
        Ok(path) => {
            tracing::debug!(path = %path.display(), "Metrics snapshot updated");
        }
        Err(error) => {
            tracing::warn!(error = %error, "Failed to persist metrics snapshot");
        }
    }

    if config.notifications.enabled {
        let notifier = automated_flywheel_setup_checker::reporting::Notifier::new(
            config.notifications.to_internal(),
        );
        let (title, body) = build_notification_summary(&results, &run_id, started_at);
        if let Err(error) = notifier.notify(&title, &body, any_failed).await {
            tracing::warn!(error = %error, "Notification delivery failed");
        }
    }

    if any_failed {
        std::process::exit(1);
    }

    Ok(())
}

fn cmd_list(
    config: &automated_flywheel_setup_checker::Config,
    enabled_only: bool,
    tag: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    let checksums_path = config.general.acfs_repo.join("checksums.yaml");

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
    }

    let checksums = parse_checksums(&checksums_path)?;

    let mut filtered: Vec<_> = checksums
        .installers
        .iter()
        .filter(|(_, entry)| {
            if enabled_only && !entry.enabled {
                return false;
            }
            if let Some(ref t) = tag {
                if !entry.tags.contains(t) {
                    return false;
                }
            }
            true
        })
        .collect();
    filtered.sort_by(|a, b| a.0.cmp(b.0));

    match format {
        OutputFormat::Human => {
            println!("Installers ({}):", filtered.len());
            for (name, entry) in &filtered {
                let status = if entry.enabled { "enabled" } else { "disabled" };
                let tags = if entry.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", entry.tags.join(", "))
                };
                let has_checksum = if entry.sha256.is_some() { " sha256" } else { "" };
                println!("  {} - {}{}{}", name, status, has_checksum, tags);
            }
        }
        OutputFormat::Json => {
            let output: Vec<_> = filtered
                .iter()
                .map(|(name, entry)| {
                    serde_json::json!({
                        "name": name,
                        "url": entry.url,
                        "sha256": entry.sha256,
                        "enabled": entry.enabled,
                        "tags": entry.tags,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            for (name, entry) in &filtered {
                let output = serde_json::json!({
                    "kind": "installer",
                    "schema_version": SCHEMA_VERSION,
                    "name": name,
                    "url": entry.url,
                    "sha256": entry.sha256,
                    "enabled": entry.enabled,
                    "tags": entry.tags,
                });
                println!("{}", serde_json::to_string(&output)?);
            }
        }
    }

    Ok(())
}

fn cmd_status(detailed: bool, format: OutputFormat) -> Result<()> {
    use automated_flywheel_setup_checker::reporting::{
        MetricsExporter, MetricsSnapshot, ResultPersister,
    };

    if matches!(format, OutputFormat::Prometheus) {
        let mut snapshot = MetricsSnapshot::load_or_default(&MetricsSnapshot::default_path());
        snapshot.reset_if_stale();
        let exporter = MetricsExporter::from_snapshot("afsc", &snapshot);
        print!("{}", exporter.export());
        return Ok(());
    }

    let persister = ResultPersister::default_dir();

    let latest = persister.latest_results()?;
    let results_path = match latest {
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
                    if matches!(format, OutputFormat::Json) {
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("{}", serde_json::to_string(&output)?);
                    }
                }
            }
            return Ok(());
        }
    };

    let (entries, summary) = ResultPersister::read_results(&results_path)?;

    match format {
        OutputFormat::Human => {
            if let Some(ref s) = summary {
                println!(
                    "Last run: {} ({} total, {} passed, {} failed, {} skipped)",
                    s.run_id.chars().take(8).collect::<String>(),
                    s.total,
                    s.passed,
                    s.failed,
                    s.skipped
                );
                println!("Duration: {}ms", s.duration_total_ms);
                println!(
                    "Time: {} - {}",
                    s.timestamp_start.format("%Y-%m-%d %H:%M:%S"),
                    s.timestamp_end.format("%H:%M:%S")
                );
                println!();
            }

            for entry in &entries {
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
                "results": entries,
                "summary": summary,
                "file": results_path.to_string_lossy(),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            for entry in &entries {
                println!("{}", serde_json::to_string(&with_kind("result", entry)?)?);
            }
            if let Some(s) = &summary {
                println!("{}", serde_json::to_string(&with_kind("summary", s)?)?);
            }
        }
    }

    Ok(())
}

async fn cmd_validate(
    config: &automated_flywheel_setup_checker::Config,
    path: Option<PathBuf>,
    check_urls_flag: bool,
    check_hashes_flag: bool,
    format: OutputFormat,
) -> Result<()> {
    use automated_flywheel_setup_checker::checksums::{check_hashes, check_urls};

    let checksums_path = path.unwrap_or_else(|| config.general.acfs_repo.join("checksums.yaml"));

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
    }

    let checksums = parse_checksums(&checksums_path)?;
    let result = validate_checksums(&checksums, false); // format validation only

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
    });
    let mut exit_code = if result.valid { 0 } else { 1 };

    if matches!(format, OutputFormat::Jsonl) {
        let mut line = serde_json::json!({
            "kind": "format",
            "schema_version": SCHEMA_VERSION,
            "path": checksums_path.to_string_lossy(),
        });
        line["valid"] = report["format"]["valid"].clone();
        line["errors"] = report["format"]["errors"].clone();
        line["warnings"] = report["format"]["warnings"].clone();
        println!("{}", serde_json::to_string(&line)?);
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
                    println!("{}", serde_json::to_string(&with_kind("url_check", r)?)?);
                }
            }
        }

        if broken > 0 {
            exit_code = 1;
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
                    println!("{}", serde_json::to_string(&with_kind("hash_check", r)?)?);
                }
            }
        }

        if mismatched > 0 {
            exit_code = 1;
        }
    }

    report["exit_code"] = serde_json::json!(exit_code);
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Jsonl | OutputFormat::Prometheus => {
            let line = serde_json::json!({
                "kind": "summary",
                "schema_version": SCHEMA_VERSION,
                "valid": result.valid,
                "exit_code": exit_code,
            });
            println!("{}", serde_json::to_string(&line)?);
        }
        OutputFormat::Human => {}
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn cmd_classify_error(
    stderr: &str,
    exit_code: i32,
    explain: bool,
    format: OutputFormat,
) -> Result<()> {
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
            if matches!(format, OutputFormat::Json) {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", serde_json::to_string(&v)?);
            }
        }
    }

    Ok(())
}

fn cmd_config(cmd: ConfigCmd, config_path: &Option<PathBuf>, format: OutputFormat) -> Result<()> {
    match cmd {
        ConfigCmd::Show => {
            let config = load_config(config_path.as_deref())?;
            match format {
                OutputFormat::Human => {
                    println!("Current configuration:");
                    println!("{}", toml::to_string_pretty(&config)?);
                }
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                }
            }
        }
        ConfigCmd::Default => {
            let config = automated_flywheel_setup_checker::Config::default();
            match format {
                OutputFormat::Human => {
                    println!("Default configuration:");
                    println!("{}", toml::to_string_pretty(&config)?);
                }
                OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Prometheus => {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                }
            }
        }
        ConfigCmd::Validate => match config_path {
            Some(path) => match load_config(Some(path)) {
                Ok(_) => println!("Configuration file is valid: {:?}", path),
                Err(e) => {
                    eprintln!("Configuration file is invalid: {}", e);
                    std::process::exit(1);
                }
            },
            None => {
                println!("No configuration file specified, using defaults");
            }
        },
    }

    Ok(())
}

/// Parse a human-readable memory limit string (e.g., "2G", "512M") into bytes
fn parse_memory_limit(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = if s.ends_with('G') || s.ends_with('g') {
        (&s[..s.len() - 1], 1024 * 1024 * 1024u64)
    } else if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1024 * 1024u64)
    } else if s.ends_with('K') || s.ends_with('k') {
        (&s[..s.len() - 1], 1024u64)
    } else {
        (s, 1u64)
    };
    num_str.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

fn persist_metrics_snapshot(
    results: &[automated_flywheel_setup_checker::runner::TestResult],
    remediation_attempted: bool,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<std::path::PathBuf> {
    let path = automated_flywheel_setup_checker::reporting::MetricsSnapshot::default_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut snapshot =
        automated_flywheel_setup_checker::reporting::MetricsSnapshot::load_or_default(&path);
    snapshot.reset_if_stale();

    for result in results {
        snapshot.record_test(result.success);
    }

    if remediation_attempted {
        snapshot.record_remediation();
    }

    let uptime_seconds = (chrono::Utc::now() - started_at).num_seconds().max(0) as u64;
    snapshot.set_uptime(uptime_seconds);
    snapshot.save(&path)?;

    Ok(path)
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
    fn base_image_timeout_has_heavy_setup_floor() {
        assert_eq!(base_image_timeout_seconds(60), HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS);
        assert_eq!(base_image_timeout_seconds(1_200), 1_200);
    }

    #[test]
    fn mdwb_timeout_has_heavy_setup_floor() {
        assert_eq!(installer_timeout_seconds("mdwb", 300), HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS);
        assert_eq!(installer_timeout_seconds("mdwb", 1_200), 1_200);
    }

    #[test]
    fn ordinary_installer_timeout_is_unchanged() {
        assert_eq!(installer_timeout_seconds("dcg", 300), 300);
    }

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
        assert_eq!(s["failed"], 4);
        assert_eq!(s["timed_out"], 1);
        assert_eq!(s["skipped"], 1);
        assert_eq!(s["cancelled"], 1);
    }
}
