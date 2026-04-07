# Automated Flywheel Setup Checker

<div align="center">

[![CI](https://img.shields.io/github/actions/workflow/status/Dicklesworthstone/automated_flywheel_setup_checker/ci.yml?style=for-the-badge&label=CI)](https://github.com/Dicklesworthstone/automated_flywheel_setup_checker/actions/workflows/ci.yml)
[![E2E Tests](https://img.shields.io/github/actions/workflow/status/Dicklesworthstone/automated_flywheel_setup_checker/e2e-tests.yml?style=for-the-badge&label=E2E)](https://github.com/Dicklesworthstone/automated_flywheel_setup_checker/actions/workflows/e2e-tests.yml)
![Version](https://img.shields.io/badge/Version-0.1.0-bd93f9?style=for-the-badge)
![Language](https://img.shields.io/badge/Language-Rust-f74c00?style=for-the-badge)
![License](https://img.shields.io/badge/License-MIT-yellow?style=for-the-badge)

**Automated verification of [ACFS](https://github.com/Dicklesworthstone/agentic_coding_flywheel_setup) installer scripts in isolated Docker containers — with error classification, parallel execution, and Claude-powered auto-remediation.**

</div>

---

## TL;DR

**The Problem:** ACFS ships 41 installer scripts that download, verify, and configure tools on fresh Ubuntu VPS instances. Any upstream URL change, checksum drift, or dependency issue silently breaks the installer for all users. Manual testing across all tools is tedious and error-prone.

**The Solution:** This tool runs each installer inside an isolated Docker container, classifies any failures automatically, retries transient errors with exponential backoff, can optionally ask Claude to suggest fixes, and can expose the latest persisted health and metrics snapshot for monitoring.

### Why Use This?

| Feature | What It Does |
|---------|--------------|
| **Isolated Docker Testing** | Each installer runs in a fresh `ubuntu:22.04` container — no host contamination |
| **Error Classification** | Automatically categorizes failures: network, permission, dependency, configuration, resource |
| **Parallel Execution** | Run N installer tests concurrently with configurable worker count |
| **Retry with Backoff** | Transient failures (network timeouts, rate limits) are retried automatically |
| **Claude Auto-Remediation** | Experimental: sends failure context to Claude for fix suggestions |
| **JSONL Structured Output** | Machine-readable output for CI/CD pipelines and dashboards |
| **Monitoring Server** | `serve` exposes `/health` and `/metrics` from the persisted metrics snapshot |
| **Prometheus Status Export** | `status --format prometheus` emits scrape-friendly metrics to stdout for one-shot export workflows |
| **Systemd Watchdog** | Optional readiness, status, and watchdog notifications for long-running `check` and `serve` processes |
| **Checksum Validation** | Verifies checksums.yaml integrity and URL accessibility |

---

## Quick Example

```bash
# List all 41 ACFS installers
automated_flywheel_setup_checker list

# Validate checksums.yaml format and check all URLs are live
automated_flywheel_setup_checker validate --check-urls

# Dry run locally — see what would be tested without starting Docker containers
automated_flywheel_setup_checker check --dry-run --local

# Test specific installers
automated_flywheel_setup_checker check rust nodejs bun

# Run all enabled installers with 4 parallel workers
automated_flywheel_setup_checker check --parallel 4

# Classify an error message (useful for debugging)
automated_flywheel_setup_checker classify-error \
  --stderr "curl: (7) Failed to connect: Connection refused" \
  --exit-code 7

# Full run with remediation suggestions, stop on first failure
automated_flywheel_setup_checker check --remediate --fail-fast --format jsonl

# After enabling [monitoring] in config, serve health and metrics endpoints
automated_flywheel_setup_checker serve --health-port 8080

# Export the latest metrics snapshot in Prometheus text format
automated_flywheel_setup_checker status --format prometheus
```

---

## How It Compares

| Feature | This Tool | Manual SSH Testing | CI-only Canary |
|---------|-----------|-------------------|----------------|
| Isolation | Docker containers | Real VPS (risky) | VM-based (slow) |
| Parallelism | Configurable workers | Sequential | Limited |
| Error Classification | Automatic (6 categories) | Human reads logs | Grep patterns |
| Auto-Remediation | Claude-powered suggestions | N/A | N/A |
| Cost per run | Free (local Docker) | VPS hourly cost | GH Actions minutes |
| Setup time | `cargo build` | Provision VPS | Write workflow |
| Feedback loop | Seconds | Minutes | Minutes |

**When to use this tool:**
- Verifying all ACFS installers still work after upstream changes
- Testing checksums.yaml modifications before committing
- Debugging a specific installer failure with detailed classification
- Running as a scheduled check (via systemd timer) to catch regressions

**When this tool might not be ideal:**
- Testing the full ACFS install experience end-to-end (use the [installer canary workflow](https://github.com/Dicklesworthstone/agentic_coding_flywheel_setup/actions/workflows/installer-canary-strict.yml) for that)
- Verifying post-install configuration (this tests downloads + execution, not full system setup)

---

## Installation

### From Source (Recommended)

```bash
git clone https://github.com/Dicklesworthstone/automated_flywheel_setup_checker.git
cd automated_flywheel_setup_checker
cargo build --release
cp target/release/automated_flywheel_setup_checker ~/.local/bin/
```

### Direct Build

```bash
cargo install --git https://github.com/Dicklesworthstone/automated_flywheel_setup_checker.git
```

### Requirements

- **Rust nightly** (pinned via `rust-toolchain.toml`; automatically installed by `rustup`)
- **Docker** (for running isolated installer tests)
- **ACFS repository** cloned locally (default: `/data/projects/agentic_coding_flywheel_setup`)

---

## Commands

Global flags available on all commands:

```bash
--format human|json|jsonl|prometheus   # prometheus is supported by `status`
--config PATH                # Config file path (or set ACFS_CONFIG env)
-v / -vv / -vvv              # Verbosity level
--watchdog                   # Enable systemd watchdog integration
```

### `check` — Run Installer Tests

Executes installers inside Docker containers and reports results.

```bash
automated_flywheel_setup_checker check                     # All enabled installers
automated_flywheel_setup_checker check rust nodejs bun      # Specific installers
automated_flywheel_setup_checker check --parallel 4         # 4 concurrent workers
automated_flywheel_setup_checker check --timeout 600        # 10min timeout per installer
automated_flywheel_setup_checker check --remediate          # Enable Claude auto-fix
automated_flywheel_setup_checker check --fail-fast          # Stop on first failure
automated_flywheel_setup_checker check --dry-run            # Preview without executing
automated_flywheel_setup_checker check --local              # Run locally (no Docker)
```

### `list` — Show Available Installers

```bash
automated_flywheel_setup_checker list                       # All installers
automated_flywheel_setup_checker list --enabled-only        # Only enabled ones
automated_flywheel_setup_checker list --tag essential        # Filter by tag
automated_flywheel_setup_checker list --format json         # Machine-readable
```

### `validate` — Check checksums.yaml

```bash
automated_flywheel_setup_checker validate                   # Format validation
automated_flywheel_setup_checker validate --check-urls      # Also verify URLs are live
automated_flywheel_setup_checker validate --path /custom/path/checksums.yaml
```

### `classify-error` — Debug Error Classification

```bash
automated_flywheel_setup_checker classify-error \
  --stderr "E: Unable to locate package foo" \
  --exit-code 100
```

### `config` — Configuration Management

```bash
automated_flywheel_setup_checker config show                # Current config
automated_flywheel_setup_checker config default             # Print defaults
automated_flywheel_setup_checker config validate            # Validate config file
```

### `status` — Last Run Results

```bash
automated_flywheel_setup_checker status                     # Summary
automated_flywheel_setup_checker status --detailed          # Full failure info
automated_flywheel_setup_checker status --format prometheus # Prometheus text output
```

### `serve` — Monitoring Endpoints

Runs the monitoring HTTP server and exposes the endpoints enabled in `[monitoring]`. At least one of
`health_endpoint` or `metrics_enabled` must be enabled.

```bash
automated_flywheel_setup_checker serve                      # Use ports from config.toml
automated_flywheel_setup_checker serve --health-port 8081   # Override shared listener port
automated_flywheel_setup_checker serve --metrics-port 9191  # Override metrics-only listener
```

`serve` returns an error if both `[monitoring].health_endpoint` and `[monitoring].metrics_enabled` are disabled.

---

## Configuration

Create a `config.toml` (or set `ACFS_CONFIG` env var):

```toml
[general]
# Path to the ACFS repository containing checksums.yaml
acfs_repo = "/data/projects/agentic_coding_flywheel_setup"
log_level = "info"   # trace, debug, info, warn, error

[docker]
image = "ubuntu:22.04"        # Base image for test containers
memory_limit = "2G"           # Per-container memory cap
cpu_quota = 1.0               # CPU cores per container
timeout_seconds = 300          # Per-installer timeout (5 min)
pull_policy = "if-not-present" # always, if-not-present, never

[execution]
parallel = 1            # Workers (1 = sequential)
retry_transient = 3     # Retries for network/transient errors
fail_fast = false       # Stop on first failure?

[remediation]
enabled = false          # Enable Claude auto-remediation
auto_commit = false      # Auto-commit suggested fixes
create_pr = true         # Create PRs for fixes
max_attempts = 3         # Max remediation attempts per failure

[notifications]
enabled = false
slack_webhook_env = "SLACK_WEBHOOK_URL"  # Env var holding the Slack webhook URL
slack_channel = ""       # Optional channel override
github_token_env = "GITHUB_TOKEN"        # Env var holding the GitHub token
github_issue_repo = ""   # e.g., "Dicklesworthstone/agentic_coding_flywheel_setup"
notify_on_failure = true
notify_on_success = false

[monitoring]
health_endpoint = false  # Enable GET /health on the monitoring server
health_port = 8080       # Shared listener port when health is enabled
metrics_enabled = false  # Enable GET /metrics on the monitoring server
metrics_port = 9090      # Listener port when serving metrics without /health

[watchdog]
default_interval_seconds = 120  # Fallback interval when systemd env vars are absent
log_pings = false               # Promote watchdog ping logs from debug to info
```

Notification secrets come from environment variables named in `[notifications]`; the config file stores env var names, not the secret values themselves.

Run `automated_flywheel_setup_checker serve` to expose the HTTP monitoring endpoints configured in `[monitoring]`. The monitoring server uses a single listener: if `health_endpoint = true`, it binds `health_port`; otherwise it uses `metrics_port`. If you only need a one-shot scrape target, `automated_flywheel_setup_checker status --format prometheus` prints the same metrics snapshot to stdout.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           CLI (clap)                                │
│   check | serve | list | validate | classify-error | config | status│
└──────────────────────────────┬──────────────────────────────────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────────────┐
│ Checksums Module │ │ Config Loader    │ │ Error Classifier         │
│ parse YAML       │ │ TOML schema      │ │ regex patterns → category│
│ validate URLs    │ │ env overrides    │ │ (network, perm, dep...)  │
└──────────────────┘ └──────────────────┘ └──────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      Test Runner (Tokio)                            │
│   ┌────────────┐  ┌────────────┐  ┌────────────┐                   │
│   │ Worker 1   │  │ Worker 2   │  │ Worker N   │  (parallel pool)  │
│   │ Docker API │  │ Docker API │  │ Docker API │                   │
│   │ (Bollard)  │  │ (Bollard)  │  │ (Bollard)  │                   │
│   └────────────┘  └────────────┘  └────────────┘                   │
│        │  retry w/ exponential backoff                              │
│        ▼                                                            │
│   ┌──────────────────────────────────────────┐                      │
│   │ Docker Container (ubuntu:22.04)          │                      │
│   │  → download installer script             │                      │
│   │  → verify checksum                       │                      │
│   │  → execute installer                     │                      │
│   │  → capture stdout/stderr/exit code       │                      │
│   └──────────────────────────────────────────┘                      │
└──────────────────────────────┬──────────────────────────────────────┘
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────────────┐
│ Remediation      │ │ Reporting        │ │ Watchdog                 │
│ Claude API       │ │ JSONL output     │ │ systemd integration      │
│ safety checks    │ │ metrics/notify   │ │ health pings             │
│ circuit breaker  │ │ summaries        │ │                          │
└──────────────────┘ └──────────────────┘ └──────────────────────────┘
```

---

## Relationship to ACFS

This tool is a **companion testing layer** for [Agentic Coding Flywheel Setup (ACFS)](https://github.com/Dicklesworthstone/agentic_coding_flywheel_setup). It does not replace ACFS's own CI workflows — it complements them:

| Layer | Tool | What It Tests |
|-------|------|---------------|
| **Unit** | This tool (`check`) | Individual installers in Docker isolation |
| **Integration** | ACFS canary workflows | Full install flow in Ubuntu VMs |
| **Production** | ACFS `acfs doctor` | Health of a live ACFS installation |

This tool reads ACFS's `checksums.yaml` as its source of truth for what installers exist and what their expected checksums are.

---

## Error Classification Categories

The classifier automatically categorizes failures for triage:

| Category | Example Patterns | Typical Fix |
|----------|-----------------|-------------|
| **Network** | Connection refused, DNS failure, timeout | Retry (automatic) |
| **Permission** | Permission denied, EACCES | Check container user/sudo |
| **Dependency** | Package not found, unmet dependencies | Update package lists |
| **Configuration** | Invalid config, missing env var | Fix installer script |
| **Resource** | Out of memory, disk full | Increase container limits |
| **Command** | Command not found (exit 127) | Install prerequisite tools |

---

## Troubleshooting

### "Docker daemon not running"

```bash
# Start Docker
sudo systemctl start docker

# Or if using Docker Desktop
open -a Docker
```

### "checksums.yaml not found"

The tool looks for ACFS at `/data/projects/agentic_coding_flywheel_setup` by default. Override with config:

```bash
automated_flywheel_setup_checker --config my-config.toml check
# Or set in config.toml: [general] acfs_repo = "/your/path"
```

### "Container timeout after 300s"

Some installers (e.g., Rust) need more time. Increase the timeout:

```bash
automated_flywheel_setup_checker check rust --timeout 600
```

### "Permission denied when connecting to Docker socket"

```bash
# Add your user to the docker group
sudo usermod -aG docker $USER
# Then log out and back in
```

### Tests pass locally but fail in CI

The E2E tests require Docker-in-Docker. Ensure the CI workflow has the `docker:dind` service and `--privileged` flag.

---

## Limitations

- **Linux-only Docker testing** — installer scripts target Ubuntu, so containers are Ubuntu-based. macOS/Windows installers aren't testable this way.
- **Network-dependent** — many installers download from the internet. Air-gapped testing isn't currently supported.
- **Claude remediation is experimental** — auto-fix suggestions require an API key and may not always be actionable. Safety checks prevent dangerous commands.
- **No post-install validation** — verifies the installer runs successfully but doesn't test that the installed tool actually works correctly.
- **Single Ubuntu version** — defaults to `ubuntu:22.04`. Testing across multiple Ubuntu versions requires manual config changes.
- **Local mode available** — use `--local` flag to run installers in temp directories when Docker is unavailable (less isolation but no Docker dependency).
- **Metrics are snapshot-based** — `serve` and `status --format prometheus` report the persisted metrics snapshot from recent runs; they do not stream live per-installer progress.

---

## FAQ

### How is this different from the ACFS canary workflows?

The canary workflows in ACFS test the **full install experience** end-to-end in a VM. This tool tests **individual installers** in lightweight Docker containers, giving faster feedback and better error isolation.

### Does it actually run Docker containers?

Yes. It uses the [Bollard](https://github.com/fussybeaver/bollard) crate to talk to the Docker API directly. Each installer gets its own fresh container.

### What does the Claude remediation actually do?

When an installer fails and `--remediate` is enabled, the tool sends the failure context (stderr, exit code, installer name) to Claude and asks for a fix suggestion. Safety checks prevent any dangerous commands from being applied. It's experimental.

### Can I run this in CI?

Yes. The E2E workflow already demonstrates this. You need a runner with Docker access. See `.github/workflows/e2e-tests.yml`.

### Why not just use `shellcheck` on the installer scripts?

`shellcheck` catches syntax and style issues but can't detect runtime failures like broken download URLs, checksum drift, or missing upstream packages. This tool actually executes the scripts.

---

## Development

```bash
# Run Rust tests (366 total; 6 Docker-dependent E2E cases are ignored by default)
cargo test

# Run with verbose logging
RUST_LOG=debug cargo run -- check --dry-run

# Format and lint
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings

# Run the full E2E suite (requires Docker)
chmod +x scripts/e2e/run_all_tests.sh
./scripts/e2e/run_all_tests.sh
```

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

MIT
