//! Automated ACFS installer verification system CLI

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

use automated_flywheel_setup_checker::{
    checksums::{parse_checksums, validate_checksums},
    config::load_config,
    parser::classify_error,
    runner::{InstallerTest, InstallerTestRunner, RunnerConfig},
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
        Commands::Check { installers, parallel, timeout, dry_run, remediate, fail_fast } => {
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
            cmd_validate(config, path.clone(), *check_urls, cli.format)?;
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
                println!();
                for (name, entry) in &enabled {
                    println!("  - {} (v{})", name, entry.version.as_deref().unwrap_or("unknown"));
                }
            }
            OutputFormat::Json | OutputFormat::Jsonl => {
                let output = serde_json::json!({
                    "dry_run": true,
                    "installers": enabled.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                    "parallel": parallel,
                    "timeout": timeout,
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        return Ok(());
    }

    // Set up the runner with configuration
    let runner_config = RunnerConfig {
        default_timeout: Duration::from_secs(timeout),
        dry_run: true, // Run installers with --dry-run for safety
        ..Default::default()
    };
    let runner = InstallerTestRunner::new(runner_config);

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

    // Run tests (sequentially for now, parallel support via ParallelRunner)
    let mut results = Vec::new();
    let mut any_failed = false;

    for test in &tests {
        let result = runner.run_test_with_retry(test).await?;

        if !result.success {
            any_failed = true;
        }

        match format {
            OutputFormat::Human => {
                let status_icon = if result.success { "✓" } else { "✗" };
                println!(
                    "{} {} ({:?}, {}ms)",
                    status_icon,
                    result.installer_name,
                    result.status,
                    result.duration_ms
                );
                if !result.success && !result.stderr.is_empty() {
                    // Show first few lines of stderr
                    let stderr_preview: String = result
                        .stderr
                        .lines()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!("    stderr: {}", stderr_preview);
                }
            }
            OutputFormat::Json => {
                // Collect for final JSON output
            }
            OutputFormat::Jsonl => {
                println!("{}", serde_json::to_string(&result)?);
            }
        }

        results.push(result);

        if fail_fast && any_failed {
            break;
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
                let version = entry.version.as_deref().unwrap_or("?");
                let tags = if entry.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", entry.tags.join(", "))
                };
                println!("  {} ({}) - {}{}", name, version, status, tags);
            }
        }
        OutputFormat::Json => {
            let output: Vec<_> = filtered
                .iter()
                .map(|(name, entry)| {
                    serde_json::json!({
                        "name": name,
                        "version": entry.version,
                        "enabled": entry.enabled,
                        "tags": entry.tags,
                        "url": entry.url,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Jsonl => {
            for (name, entry) in &filtered {
                let output = serde_json::json!({
                    "name": name,
                    "version": entry.version,
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
    match format {
        OutputFormat::Human => {
            println!("Last run status:");
            println!("  Status: No runs recorded yet");
            if detailed {
                println!("  (Run 'check' command first to generate status)");
            }
        }
        OutputFormat::Json | OutputFormat::Jsonl => {
            let output = serde_json::json!({
                "status": "no_runs",
                "message": "No runs recorded yet"
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }
    Ok(())
}

fn cmd_validate(
    config: &automated_flywheel_setup_checker::Config,
    path: Option<PathBuf>,
    check_urls: bool,
    format: OutputFormat,
) -> Result<()> {
    let checksums_path = path.unwrap_or_else(|| config.general.acfs_repo.join("checksums.yaml"));

    if !checksums_path.exists() {
        anyhow::bail!("checksums.yaml not found at {:?}", checksums_path);
    }

    let checksums = parse_checksums(&checksums_path)?;
    let result = validate_checksums(&checksums, check_urls);

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
