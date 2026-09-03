# Changelog

All notable changes to `automated_flywheel_setup_checker`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer. Output schemas
(`schema_version` on every JSON/JSONL document) only change additively within a major version.

## [Unreleased]

### Added
- `check`: verified installer execution loop against a prepared, non-root image
  (`afsc-base:<hash>` derived from `docker/Dockerfile.base` for any base), SHA-256 verification
  before execution, failure classification on every failure, bounded and redacted captures,
  cancellation on SIGINT/SIGTERM (exit 130/143), fail-fast, retries with recorded attempts,
  `--failed-from`, `--reap`, `--rebuild-base`, `--dry-run`, run lock, longest-first ordering and a
  whole-run deadline.
- Resolved settings (`defaults < config file < AFSC_* env < CLI`), `config show --resolved`,
  per-installer overrides (`[installers.<name>]`), built-in ACFS execution profiles.
- Results: unique per-run files with a run header (image id, environment, checksums hash),
  `status --list/--run/--history/--diff`, `--format markdown`, flakiness (Beta posterior) and
  broken-since (CUSUM) labels, structured event log with retention.
- Metrics and health recomputed from persisted runs (`status --format prometheus`, `serve`
  `/metrics` with per-installer gauges, `/health` with stale 503).
- Notifications: GitHub rolling issue (create → comment → close), Slack Block Kit, modes
  `every_run` / `on_change` / `daily_digest`, `notify --last-run|--run|--digest`.
- `validate --check-urls/--check-hashes/--profile` with ACFS cross-checks and drift persistence.
- `remediate checksums`: deterministic checksum refresh with a script ledger, drift risk scoring,
  in-container verification, advisory diff or worktree branch/PR; `check --remediate` attaches
  honest outcomes (`verified`, `advised`, `proposed`, `applied`, `failed`, `skipped`) and invokes
  Claude read-only with safety annotation of suggested commands.
- Claude `propose` / `apply` modes: an edit session in a git worktree of the ACFS checkout
  (`acceptEdits`, edit tools limited to the worktree, Bash opt-in via `allow_bash`) gated by a
  transcript safety scan, an edit policy (`checksums.yaml`, the `KNOWN_INSTALLERS` block,
  `scripts/generated/`) and a re-run of the installer before anything is committed; rejected
  sessions leave no worktree or branch behind. Fake `gh` and new fake-claude scenarios cover
  the branch/PR, rejection and push paths.
- Claude CLI envelopes are read even on non-zero exits: a `--max-budget-usd` / `--max-turns`
  cap is reported with the CLI's own reason and cost and is never retried (the real CLI
  previously surfaced as an empty error after three blind retries). Default
  `cost_limit_usd` is 3.0: one invocation costs ~$0.13 before it reads anything.
- `doctor` with fix hints; `--version` with git sha, build date and toolchain.
- systemd unit templates with an install script, a serve unit and a drift test; nightly canary
  and ACFS PR-gate workflows; Docker suite and real-installer job in CI; bash E2E scripts that
  drive the binary.

### Changed
- Dependencies: bollard 0.16 → 0.21 (query-parameter builders, `ContainerCreateBody`), reqwest
  0.12 → 0.13 (`rustls` feature), similar 2 → 3.
- Default base image is `ubuntu:24.04` everywhere (config default, canonical `afsc-base` tag,
  `docker/Dockerfile.base`, README examples): ACFS targets 24.04+, and on 22.04 installers that
  ship glibc 2.38+ binaries (cass, fsfs) fall back to source builds that blow the timeout.
  `ubuntu:22.04` is still supported as a foreign base (`afsc-prepared:ubuntu-22.04-<hash>`).
- `remediate checksums` prints the redacted stderr tail and a hint for every failed or timed-out
  verification instead of a bare status.
- Base image gains `minisign` and `libsqlite3-dev`, mirroring ACFS's "required apt packages"
  step (the mcp_agent_mail and caam installers fail closed without minisign); the ACFS drift
  test now checks that step as well as `install_base.sh`.
- Classifier: "X is required … but was not found" is a `dependency` failure (golden corpus entry
  from the real mcp_agent_mail output).
- Statuses serialize lowercase everywhere (`passed`, `failed`, `timedout`, `cancelled`, `skipped`).
- stdout carries only data; logs go to stderr (`--log-format json` available).
- `--local` requires a terminal, `--yes` or `AFSC_ALLOW_LOCAL=1`.
- Exit codes: 0 success, 1 installer failures, 2 usage/config, 3 infrastructure, 4 validation
  drift, 130/143 interrupted.

### Removed
- `--dangerously-skip-permissions` from every Claude invocation; the unsafe auto-apply path.
- `list --tag/--enabled-only`; the Docker-in-Docker CI service; the log-grepping notify script
  (kept as a thin wrapper around `notify --last-run`).
