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

**The Problem:** ACFS ships 48 installer scripts (as of September 2026) that download, verify, and configure tools on fresh Ubuntu VPS instances. Any upstream URL change, checksum drift, or dependency issue silently breaks the installer for all users. Manual testing across all tools is tedious and error-prone.

**The Solution:** This tool runs each installer inside an isolated Docker container, classifies any failures automatically, retries transient errors with exponential backoff, can optionally ask Claude to suggest fixes, and can expose the latest persisted health and metrics snapshot for monitoring.

### Why Use This?

| Feature | What It Does |
|---------|--------------|
| **Isolated Docker Testing** | Each installer runs in a fresh `ubuntu:24.04` container (the ACFS target release) — no host contamination |
| **Error Classification** | Automatically categorizes failures: network, permission, dependency, configuration, resource |
| **Parallel Execution** | Run N installer tests concurrently with configurable worker count |
| **Retry with Backoff** | Transient failures (network timeouts, rate limits) are retried automatically |
| **Claude Remediation** | Read-only advice, or a verified branch/PR from a policy-gated edit session (`advisory` / `propose` / `apply`) |
| **JSONL Structured Output** | Machine-readable output for CI/CD pipelines and dashboards |
| **Monitoring Server** | `serve` exposes `/health` and `/metrics` from the persisted metrics snapshot |
| **Prometheus Status Export** | `status --format prometheus` emits scrape-friendly metrics to stdout for one-shot export workflows |
| **Systemd Watchdog** | Optional readiness, status, and watchdog notifications for long-running `check` and `serve` processes |
| **Checksum Validation** | Verifies checksums.yaml integrity, URL accessibility, and current installer hashes |

---

## Quick Example

```bash
# List all ACFS installers (48 today)
automated_flywheel_setup_checker list

# Validate checksums.yaml format, URL reachability, and pinned hashes
automated_flywheel_setup_checker validate --check-urls --check-hashes

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
--format human|json|jsonl|prometheus|markdown   # prometheus and markdown: `status` only
--config PATH                # Config file path (or set ACFS_CONFIG env)
--acfs-repo DIR              # Override [general].acfs_repo
--data-dir DIR               # Override [general].data_dir (results, metrics, locks, logs)
--image NAME                 # Override [docker].image
--allow-file-urls            # Permit file:// installer URLs (tests, local mirrors)
-v / -vv / -vvv, --quiet     # Log verbosity (logs go to stderr; stdout is data only)
--log-format text|json       # stderr log line format
--watchdog                   # systemd sd_notify + watchdog integration
```

Every `AFSC_<SECTION>_<KEY>` environment variable overrides the matching config key
(`AFSC_EXECUTION_PARALLEL=4`, `AFSC_DOCKER_IMAGE=ubuntu:24.04`); CLI flags override both.

Exit codes: `0` success · `1` installer failures · `2` usage/config error (including an invalid
`checksums.yaml`) · `3` infrastructure error (Docker unreachable, lock held) · `4` validation drift ·
`130`/`143` interrupted by SIGINT/SIGTERM.

### `check` — Run Installer Tests

Downloads each installer, verifies its pinned SHA-256, runs it inside a fresh container (or, with
`--local`, in an isolated temporary HOME on this host) and classifies every failure.

```bash
automated_flywheel_setup_checker check                     # All enabled installers
automated_flywheel_setup_checker check rust nodejs bun      # Specific installers
automated_flywheel_setup_checker check --parallel 4         # 4 concurrent workers ("auto" = CPUs)
automated_flywheel_setup_checker check --timeout 600        # 10 min timeout per installer
automated_flywheel_setup_checker check --fail-fast          # Stop on first failure
automated_flywheel_setup_checker check --dry-run            # Resolved specs, order, commands; runs nothing
automated_flywheel_setup_checker check --failed-from last   # Rerun only what failed in a run (id prefix or "last")
automated_flywheel_setup_checker check --rebuild-base       # Rebuild the prepared image (pull base, no cache)
automated_flywheel_setup_checker check --reap               # Only remove leaked afsc containers
automated_flywheel_setup_checker check --local --yes        # Run on this host (consent required when not a TTY)
automated_flywheel_setup_checker check --remediate          # Claude remediation per [remediation].mode
```

Installers run longest-first (by historical median duration) unless `[execution].order` says
otherwise; `[execution].run_deadline_seconds` cancels whatever is still running at the deadline
(reported as `cancelled`, exit 0 unless something actually failed). Only one `check` runs per data
dir at a time (`--allow-concurrent` to override).

### `status` — Results, History, Diffs

```bash
automated_flywheel_setup_checker status                     # Last run (flaky / broken-since labels)
automated_flywheel_setup_checker status --detailed          # Attempts, categories, checksum state
automated_flywheel_setup_checker status --list              # Recent runs
automated_flywheel_setup_checker status --run 3f2a          # A run by id prefix
automated_flywheel_setup_checker status --history rust      # One installer across runs + assessment
automated_flywheel_setup_checker status --diff 3f2a last    # Installers whose status changed
automated_flywheel_setup_checker status --format markdown   # Tables for issues / PR comments
automated_flywheel_setup_checker status --format prometheus # Same numbers `serve` exposes
```

Flakiness uses a Beta(1,1) posterior over outcomes since the installer's script hash last changed
(flaky below a 90 % pass probability with intermittent failures); a CUSUM change point marks
`broken since <run>`.

### `list` — Show Installers

```bash
automated_flywheel_setup_checker list                       # Resolved specs (profile, overrides, skips)
automated_flywheel_setup_checker list --runnable            # Excludes [installers.<name>].skip = true
automated_flywheel_setup_checker list --format json
```

### `validate` — Check checksums.yaml

```bash
automated_flywheel_setup_checker validate                   # Format validation (exit 2 on errors)
automated_flywheel_setup_checker validate --check-urls      # URLs reachable (HEAD, GET fallback)
automated_flywheel_setup_checker validate --check-hashes    # Download and compare pinned SHA-256 (exit 4 on drift)
automated_flywheel_setup_checker validate --profile         # Built-in ACFS execution profiles vs the checkout
automated_flywheel_setup_checker validate --path /custom/path/checksums.yaml
```

Against a full ACFS checkout, `validate` also cross-checks `KNOWN_INSTALLERS` in
`scripts/lib/security.sh`. `--check-hashes` persists its result for `serve` and `doctor`.

### `remediate checksums` — Fix Drift Without Guessing

Checksum drift is the most common failure and needs no model: the tool downloads each installer,
compares the hash with the pin, diffs the served script against the last known-good copy in its
ledger (`<data_dir>/scripts/<installer>/<sha>.sh`) and scores the change (`routine` for version
bumps, `review` for logic changes, `suspicious` for new download hosts, `curl … | sh`, `base64 -d`,
`rm -rf`, `chmod 777`, `eval` or opaque blobs), then runs the installer with the new hash in a
fresh container before it will vouch for it.

```bash
automated_flywheel_setup_checker remediate checksums                  # advisory: diff + verification + candidate file
automated_flywheel_setup_checker remediate checksums --from-last-run  # only the mismatches of the last run
automated_flywheel_setup_checker remediate checksums --only uv bun
automated_flywheel_setup_checker remediate checksums --mode propose   # git worktree branch afsc/checksum-refresh-<date> (+ PR if create_pr)
automated_flywheel_setup_checker remediate checksums --mode apply     # propose + push the branch (never main)
automated_flywheel_setup_checker check --remediate                    # same refresh for drift, read-only Claude advice for the rest
```

Entries that fail verification are excluded from proposals and reported (exit 1). `check --remediate`
records an honest outcome on every failing result (`verified`, `advised`, `proposed`, `applied`,
`failed`, `skipped`): the word "succeeded" only ever appears for a verified or applied fix.

For failures that are not checksum drift, `[remediation].mode` decides how far Claude may go:

| mode | what Claude gets | what lands |
|---|---|---|
| `advisory` | read-only (`--permission-mode default --tools Read,Grep,Glob`) | advice on the result; commands it suggests are safety-flagged, never run |
| `propose` | an edit session in a git worktree of the ACFS checkout (`--permission-mode acceptEdits --tools Read,Grep,Glob,Edit,Write --add-dir <worktree>`) | a commit on `afsc/remediate-<installer>-<date>` plus a PR when `create_pr = true` |
| `apply` | same as propose | the branch is also pushed (never `main`) |

An edit session only lands if every gate passes, otherwise the worktree and branch are discarded
and the result says `failed` with the reason: (1) no High/Critical command in Claude's summary,
(2) every changed path is `checksums.yaml`, the `KNOWN_INSTALLERS` block of
`scripts/lib/security.sh` or something under `scripts/generated/`, (3) the installer passes again
when re-run through the executor against the worktree's `checksums.yaml`. Up to `max_attempts`
sessions, each told what still fails; costs are summed from the CLI envelopes. Bash is off unless
`allow_bash = true` (needed for `bun run generate`). Budget note: one `claude --print` invocation
costs about $0.13 before it reads anything, so `cost_limit_usd` below ~1 cannot finish a run; a
budget or turn cap is reported once (`failed: Reached maximum budget …`) and never retried.

Committed baselines of the whole ACFS catalog running in the prepared image live under
`docs/baseline/` (`scripts/baseline_run.sh` regenerates one: per-installer verdicts, versions,
durations, peak memory, validation state, and failure tails).

### `doctor` — Diagnose the Environment

```bash
automated_flywheel_setup_checker doctor                     # Docker, image, ACFS repo, dirs, disk, tools, last run
automated_flywheel_setup_checker doctor --local --format json
```

Each finding comes with a fix hint; exit 3 when a check fails.

### `notify` — Re-send Notifications

```bash
automated_flywheel_setup_checker notify --last-run          # For the most recent persisted run
automated_flywheel_setup_checker notify --run 3f2a
automated_flywheel_setup_checker notify --digest            # Flush runs queued by mode = "daily_digest"
```

### `serve` — Monitoring Endpoints

Serves `/health` and `/metrics`, recomputed from the data directory on every request. `/health`
answers `503` (`[monitoring].stale_status_code`) when the last run is older than
`[monitoring].stale_after_seconds`; `/metrics` includes per-installer gauges
(`afsc_installer_status{installer="rust"}`), the rolling 24 h counters and the last checksum drift.

```bash
automated_flywheel_setup_checker serve                      # Ports and bind address from config.toml
automated_flywheel_setup_checker serve --health-port 8081   # Override the shared listener port (0 = ephemeral)
```

### `classify-error` — Debug Error Classification

```bash
automated_flywheel_setup_checker classify-error --stderr "E: Unable to locate package foo" --exit-code 100 --explain
```

### `config` — Configuration Management

```bash
automated_flywheel_setup_checker config show --resolved     # Every key with its source (default/file/env/cli)
automated_flywheel_setup_checker config default             # Print defaults
automated_flywheel_setup_checker config validate --strict   # Unknown keys are errors
```

---

## Configuration

Create a `config.toml` (or set `ACFS_CONFIG` env var):

```toml
[general]
# Path to the ACFS repository containing checksums.yaml
acfs_repo = "/data/projects/agentic_coding_flywheel_setup"
log_level = "info"   # trace, debug, info, warn, error

[docker]
image = "ubuntu:24.04"        # Base image for test containers (ACFS targets 24.04+; prebuilt cass/fsfs binaries need glibc 2.38+)
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
mode = "advisory"        # advisory | propose | apply (see "remediate checksums" above)
create_pr = true         # Open a PR for propose/apply branches (needs gh)
max_attempts = 3         # Edit sessions per failure (propose/apply)
cost_limit_usd = 3.0     # Per-run Claude spend cap; one invocation costs ~$0.13 before any work
max_turns = 12           # Agent turns per invocation
timeout_seconds = 300    # Per invocation
allow_bash = false       # Let edit sessions run shell commands (bun run generate)

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
│   │ Docker Container (ubuntu:24.04)          │                      │
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

## Deployment (systemd)

`scripts/install-systemd.sh` renders `systemd/*.service.in`, installs the binary to
`/usr/local/bin`, creates `/var/lib/flywheel-checker` (results, metrics, locks) and
`/var/log/flywheel-checker` (structured event log) owned by the service user, writes
`/etc/flywheel-checker/config.toml` pointing at them, installs logrotate, and enables:

| Unit | Purpose |
|------|---------|
| `automated-flywheel-checker.timer` | Nightly at 03:00 (+ up to 30 min jitter), catches up after downtime |
| `automated-flywheel-checker.service` | `check` with sd_notify + watchdog; `ExecStopPost` re-sends notifications |
| `automated-flywheel-checker-emergency.service` | On-demand run with `--parallel 4` |
| `automated-flywheel-checker-serve.service` | `/health` and `/metrics` |

```bash
cargo build --release
sudo scripts/install-systemd.sh --dry-run --acfs-repo /srv/agentic_coding_flywheel_setup   # preview
sudo scripts/install-systemd.sh --acfs-repo /srv/agentic_coding_flywheel_setup --user afsc
journalctl -u automated-flywheel-checker.service -f
automated_flywheel_setup_checker --config /etc/flywheel-checker/config.toml status --list
scripts/uninstall-systemd.sh
```

The service user must be in the `docker` group. Notification secrets are read from the environment
variables named in `[notifications]` (`Environment=` or an `EnvironmentFile=` drop-in). A unit test
parses every `ExecStart` in the templates with the real CLI, so units cannot drift from the binary.

---

## Troubleshooting

Start with `automated_flywheel_setup_checker doctor`: it checks Docker, the prepared image, the ACFS
checkout, data/log directories, free disk, external tools, notification secrets, the last run age and
leaked containers, each with a fix hint.

### "Docker daemon unreachable"

```bash
sudo systemctl start docker          # or: open -a Docker (Docker Desktop)
sudo usermod -aG docker $USER        # then log out and back in
automated_flywheel_setup_checker check --local --yes   # no-container fallback (runs scripts on this host)
```

### "checksums.yaml not found"

Point the tool at an ACFS checkout: `--acfs-repo DIR`, `[general] acfs_repo = "DIR"` or
`AFSC_GENERAL_ACFS_REPO=DIR`.

### An installer times out

Raise the per-installer timeout (`--timeout 600`, `[docker].timeout_seconds`, or
`[installers.<name>].timeout_seconds`). The built-in ACFS profiles already give `rust` and `mdwb`
longer minimums. The whole run can be bounded with `[execution].run_deadline_seconds`.

### "another check is running"

A previous run holds `<data_dir>/locks/run.lock`. Wait for it, or pass `--allow-concurrent`. Stale
locks from dead processes are reclaimed automatically.

### Leaked containers

`check --reap` removes `afsc.managed` containers whose owner process is gone; `doctor` lists them.

---

## Limitations

- **Linux-only Docker testing** — installer scripts target Ubuntu, so containers are Ubuntu-based. macOS/Windows installers aren't testable this way.
- **Network-dependent** — many installers download from the internet. Air-gapped testing isn't currently supported.
- **Claude remediation is experimental** — auto-fix suggestions require an API key and may not always be actionable. Safety checks prevent dangerous commands.
- **No post-install validation** — verifies the installer runs successfully but doesn't test that the installed tool actually works correctly.
- **Single Ubuntu version** — defaults to `ubuntu:24.04` (older bases such as `ubuntu:22.04` make installers that ship glibc 2.38+ binaries, e.g. cass and fsfs, fall back to slow source builds). Testing across multiple Ubuntu versions requires manual config changes.
- **Local mode available** — use `--local` flag to run installers in temp directories when Docker is unavailable (less isolation but no Docker dependency).
- **Metrics are snapshot-based** — `serve` and `status --format prometheus` report the persisted metrics snapshot from recent runs; they do not stream live per-installer progress.

---

## FAQ

### How is this different from the ACFS canary workflows?

The canary workflows in ACFS test the **full install experience** end-to-end in a VM. This tool tests **individual installers** in lightweight Docker containers, giving faster feedback and better error isolation.

### Does it actually run Docker containers?

Yes. It uses the [Bollard](https://github.com/fussybeaver/bollard) crate to talk to the Docker API directly. Each installer gets its own fresh container.

### What does the Claude remediation actually do?

Checksum drift never involves a model: the tool refreshes and verifies the pin itself. For other
failures, `[remediation].mode` picks the depth. `advisory` invokes `claude` read-only with the
failure context and records the advice on the result, with any risky command flagged; `propose`
lets Claude edit inside a git worktree of the ACFS checkout and only commits a branch after the
installer passes again; `apply` also pushes that branch. Costs come from the CLI's own envelope
and are capped per run. See "remediate checksums" above for the gates and the budget note.

### Can I run this in CI?

Yes. The E2E workflow already demonstrates this. You need a runner with Docker access. See `.github/workflows/e2e-tests.yml`.

### Why not just use `shellcheck` on the installer scripts?

`shellcheck` catches syntax and style issues but can't detect runtime failures like broken download URLs, checksum drift, or missing upstream packages. This tool actually executes the scripts.

---

## Development

```bash
cargo test                                   # unit, CLI (real binary + fixtures), e2e; Docker suite skips
AFSC_DOCKER_TESTS=1 cargo test --test docker  # Docker lifecycle/image suite (needs a daemon)
AFSC_ACFS_REPO=/path/to/acfs cargo test --test acfs_drift   # profile + base-image drift vs a checkout
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
scripts/e2e/run_all_tests.sh                 # bash E2E scripts
```

Local development pins nightly (`rust-toolchain.toml`, parallel front-end flag in
`.cargo/config.toml`); the code itself needs no nightly features — CI also tests on stable, and
`.github/workflows/release.yml` builds the tagged releases (`vX.Y.Z`) on stable for
x86_64/aarch64 Linux and macOS with SHA-256 sums and a `cargo install --git` smoke test.

Remediation tests use the checked-in fake `claude` (`tests/fixtures/bin/claude`, scenarios via
`AFSC_FAKE_CLAUDE=success|unsafe|error|rate_limit|timeout`). Its envelope shape was pinned from
`claude --print --output-format json` on Claude Code 2.1.x — re-check it against a real run each
quarter and when the CLI major version changes.

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

MIT
