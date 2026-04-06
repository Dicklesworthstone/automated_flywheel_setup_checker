//! Automated ACFS installer verification system CLI

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

use automated_flywheel_setup_checker::{
    checksums::{parse_checksums, validate_checksums},
    config::load_config,
    parser::classify_error,
    runner::{
        ContainerConfig, ExecutionBackend, InstallerTest, InstallerTestRunner, PullPolicy,
        RunnerConfig,
    },
    SystemdWatchdog,
};

/// Output format for CLI commands
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
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

        /// Number of parallel checks
        #[arg(long, default_value = "1")]
        parallel: usize,

        /// Per-installer timeout in seconds
        #[arg(long, default_value = "300")]
        timeout: u64,

        /// Show what would be tested without running
        #[arg(long)]
        dry_run: bool,

        /// Enable auto-remediation on failure
        #[arg(long)]
        remediate: bool,

        /// Stop on first failure
        #[arg(long)]
        fail_fast: bool,

        /// Run locally instead of in Docker containers
        #[arg(long)]
        local: bool,
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
    },

    /// Classify an error message (for testing)
    ClassifyError {
        /// stderr content
        #[arg(long)]
        stderr: String,

        /// Exit code
        #[arg(long)]
        exit_code: i32,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    automated_flywheel_setup_checker::logging::init(cli.verbose);

    // Initialize systemd watchdog if enabled
    let watchdog = if cli.watchdog {
        let wd = Arc::new(SystemdWatchdog::new());
        // Start watchdog ping task
        let _watchdog_handle = wd.clone().start();
        Some(wd)
    } else {
        None
    };

    // Load configuration
    let config = load_config(cli.config.as_deref())?;

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
    match &cli.command {
        Commands::Check { installers, parallel, timeout, dry_run, remediate, fail_fast, local } => {
            if let Some(wd) = watchdog {
                wd.notify_status("Running installer checks");
            }
            cmd_check(
                config,
                installers.clone(),
                *parallel,
                *timeout,
                *dry_run,
                *remediate,
                *fail_fast,
                *local,
                cli.format,
            )
            .await?;
        }

        Commands::List { enabled_only, tag } => {
            cmd_list(config, *enabled_only, tag.clone(), cli.format)?;
        }

        Commands::Status { detailed } => {
            cmd_status(*detailed, cli.format)?;
        }

        Commands::Validate { path, check_urls } => {
            cmd_validate(config, path.clone(), *check_urls, cli.format).await?;
        }

        Commands::ClassifyError { stderr, exit_code } => {
            cmd_classify_error(stderr, *exit_code, cli.format)?;
        }

        Commands::Config { cmd } => {
            cmd_config(cmd.clone(), &cli.config, cli.format)?;
        }
    }

    Ok(())
}

async fn cmd_check(
    config: &automated_flywheel_setup_checker::Config,
    installers: Vec<String>,
    parallel: usize,
    timeout: u64,
    dry_run: bool,
    _remediate: bool,
    fail_fast: bool,
    local: bool,
    format: OutputFormat,
) -> Result<()> {
    use std::time::Duration;

    let checksums_path = config.general.acfs_repo.join("checksums.yaml");

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
    }

    let checksums = parse_checksums(&checksums_path)?;
    let enabled: Vec<_> = checksums
        .installers
        .iter()
        .filter(|(name, entry)| {
            entry.enabled && (installers.is_empty() || installers.contains(name))
        })
        .collect();

    if dry_run {
        match format {
            OutputFormat::Human => {
                println!(
                    "Would check {} installer(s) with {} parallel workers:",
                    enabled.len(),
                    parallel
                );
                println!("Timeout: {}s per installer", timeout);
                println!("Backend: {}", if local { "local" } else { "docker" });
                println!();
                for (name, entry) in &enabled {
                    if let Some(ver) = &entry.version {
                        println!("  - {} (v{})", name, ver);
                    } else {
                        println!("  - {}", name);
                    }
                }
            }
            OutputFormat::Json | OutputFormat::Jsonl => {
                let output = serde_json::json!({
                    "dry_run": true,
                    "installers": enabled.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                    "parallel": parallel,
                    "timeout": timeout,
                    "backend": if local { "local" } else { "docker" },
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        return Ok(());
    }

    // Select execution backend
    let backend = if local {
        ExecutionBackend::Local
    } else {
        let container_config = ContainerConfig {
            image: config.docker.image.clone(),
            memory_limit: parse_memory_limit(&config.docker.memory_limit),
            cpu_quota: Some(config.docker.cpu_quota),
            timeout_seconds: timeout,
            volumes: Vec::new(),
            environment: Vec::new(),
        };
        ExecutionBackend::Docker {
            container_config,
            pull_policy: PullPolicy::from_str(&config.docker.pull_policy),
        }
    };

    // Set up the runner with configuration
    let runner_config = RunnerConfig {
        default_timeout: Duration::from_secs(timeout),
        dry_run: false,
        backend,
        ..Default::default()
    };
    let runner = InstallerTestRunner::new(runner_config.clone());

    // Convert checksums entries to InstallerTest objects
    let tests: Vec<InstallerTest> = enabled
        .iter()
        .filter_map(|(name, entry)| {
            // Skip entries without URLs
            let url = entry.url.as_ref()?;
            let mut test = InstallerTest::new(name.as_str(), url)
                .with_timeout(Duration::from_secs(timeout));

            // Add checksum if available
            if let Some(sha256) = &entry.sha256 {
                test = test.with_sha256(sha256);
            }

            Some(test)
        })
        .collect();

    // Run tests — use parallel runner when parallel > 1
    let results = if parallel > 1 {
        use automated_flywheel_setup_checker::runner::ParallelRunner;
        let pool = ParallelRunner::new(parallel, runner_config.clone()).with_fail_fast(fail_fast);
        pool.run_all(tests).await?
    } else {
        // Sequential execution
        let mut sequential_results = Vec::new();
        for test in &tests {
            let result = runner.run_test_with_retry(test).await?;
            sequential_results.push(result);
            if fail_fast && sequential_results.last().map(|r| !r.success).unwrap_or(false) {
                break;
            }
        }
        sequential_results
    };

    let any_failed = results.iter().any(|r| !r.success);

    // Print per-result output
    for result in &results {
        match format {
            OutputFormat::Human => {
                let status_icon = if result.success { "\u{2713}" } else { "\u{2717}" };
                println!(
                    "{} {} ({:?}, {}ms)",
                    status_icon,
                    result.installer_name,
                    result.status,
                    result.duration_ms
                );
                if !result.success && !result.stderr.is_empty() {
                    let stderr_preview: String = result
                        .stderr
                        .lines()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!("    stderr: {}", stderr_preview);
                }
            }
            OutputFormat::Json => {}
            OutputFormat::Jsonl => {
                println!("{}", serde_json::to_string(&result)?);
            }
        }
    }

    // Summary output
    match format {
        OutputFormat::Human => {
            let passed = results.iter().filter(|r| r.success).count();
            let failed = results.len() - passed;
            println!();
            println!(
                "Results: {} passed, {} failed out of {} total",
                passed,
                failed,
                results.len()
            );
        }
        OutputFormat::Json => {
            let output = serde_json::json!({
                "results": results,
                "summary": {
                    "total": results.len(),
                    "passed": results.iter().filter(|r| r.success).count(),
                    "failed": results.iter().filter(|r| !r.success).count(),
                }
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl => {
            // Already printed per result
        }
    }

    // Persist results to JSONL file
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = results.first().map(|r| r.started_at).unwrap_or_else(chrono::Utc::now);
    let persister = automated_flywheel_setup_checker::reporting::ResultPersister::default_dir();
    match persister.persist(&results, &run_id, started_at) {
        Ok(path) => {
            if matches!(format, OutputFormat::Human) {
                println!("Results saved to: {}", path.display());
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to persist results: {}", e);
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

    let filtered: Vec<_> = checksums
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
        OutputFormat::Jsonl => {
            for (name, entry) in &filtered {
                let output = serde_json::json!({
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
    use automated_flywheel_setup_checker::reporting::ResultPersister;

    let persister = ResultPersister::default_dir();

    let latest = persister.latest_results()?;
    let results_path = match latest {
        Some(path) => path,
        None => {
            match format {
                OutputFormat::Human => {
                    println!("No runs found. Run: automated_flywheel_setup_checker check");
                }
                OutputFormat::Json | OutputFormat::Jsonl => {
                    let output = serde_json::json!({
                        "status": "no_runs",
                        "message": "No runs recorded yet"
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
            }
            return Ok(());
        }
    };

    let (entries, summary) = ResultPersister::read_results(&results_path)?;

    match format {
        OutputFormat::Human => {
            if let Some(ref s) = summary {
                println!("Last run: {} ({} total, {} passed, {} failed, {} skipped)",
                    s.run_id.chars().take(8).collect::<String>(),
                    s.total, s.passed, s.failed, s.skipped);
                println!("Duration: {}ms", s.duration_total_ms);
                println!("Time: {} - {}", s.timestamp_start.format("%Y-%m-%d %H:%M:%S"),
                    s.timestamp_end.format("%H:%M:%S"));
                println!();
            }

            for entry in &entries {
                let icon = match entry.status.as_str() {
                    "passed" => "\u{2713}",
                    "failed" => "\u{2717}",
                    "timedout" => "\u{29D6}",
                    "skipped" => "-",
                    _ => "?",
                };
                let checksum = if entry.sha256_verified { " sha256" } else { "" };
                println!("  {} {} ({}ms{}){}",
                    icon, entry.installer_name, entry.duration_ms,
                    if entry.retry_count > 0 { format!(", {} retries", entry.retry_count) } else { String::new() },
                    checksum);

                if detailed && !entry.stderr_excerpt.is_empty() && entry.status != "passed" {
                    let preview: String = entry.stderr_excerpt.lines().take(3)
                        .collect::<Vec<_>>().join("\n      ");
                    println!("      stderr: {}", preview);
                }
                if detailed {
                    if let Some(ref ec) = entry.error_classification {
                        println!("      error: {} ({}, retryable={}, confidence={:.0}%)",
                            ec.category, ec.severity, ec.retryable, ec.confidence * 100.0);
                    }
                }
            }

            println!("\nResults file: {}", results_path.display());
        }
        OutputFormat::Json => {
            let output = serde_json::json!({
                "results": entries,
                "summary": summary,
                "file": results_path.to_string_lossy(),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl => {
            for entry in &entries {
                println!("{}", serde_json::to_string(entry)?);
            }
            if let Some(s) = &summary {
                println!("{}", serde_json::to_string(s)?);
            }
        }
    }

    Ok(())
}

async fn cmd_validate(
    config: &automated_flywheel_setup_checker::Config,
    path: Option<PathBuf>,
    check_urls_flag: bool,
    format: OutputFormat,
) -> Result<()> {
    use automated_flywheel_setup_checker::checksums::check_urls;

    let checksums_path = path.unwrap_or_else(|| config.general.acfs_repo.join("checksums.yaml"));

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
    }

    let checksums = parse_checksums(&checksums_path)?;
    let result = validate_checksums(&checksums, false); // format validation only

    match format {
        OutputFormat::Human => {
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
        OutputFormat::Json | OutputFormat::Jsonl => {
            let output = serde_json::json!({
                "valid": result.valid,
                "errors": result.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
                "warnings": result.warnings,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }

    if !result.valid {
        std::process::exit(1);
    }

    // URL checking (async)
    if check_urls_flag {
        println!();
        println!("Checking URLs...");
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
                    let error_str = r
                        .error
                        .as_ref()
                        .map(|e| format!(" ({})", e))
                        .unwrap_or_default();
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
                let output = serde_json::json!({
                    "url_checks": url_results,
                    "summary": {
                        "total": url_results.len(),
                        "reachable": reachable,
                        "broken": broken,
                    }
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
            OutputFormat::Jsonl => {
                for r in &url_results {
                    println!("{}", serde_json::to_string(r)?);
                }
            }
        }

        if broken > 0 {
            std::process::exit(1);
        }
    }

    Ok(())
}

fn cmd_classify_error(stderr: &str, exit_code: i32, format: OutputFormat) -> Result<()> {
    let classification = classify_error(stderr, exit_code);

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
        }
        OutputFormat::Json | OutputFormat::Jsonl => {
            println!("{}", serde_json::to_string_pretty(&classification)?);
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
                OutputFormat::Json | OutputFormat::Jsonl => {
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
                OutputFormat::Json | OutputFormat::Jsonl => {
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
