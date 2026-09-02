//! Drift checks against a real ACFS checkout.
//!
//! Gated on `AFSC_ACFS_REPO=/path/to/agentic_coding_flywheel_setup` (skips otherwise, so the
//! suite stays green on machines without the checkout). CI sets it after checking out ACFS.
//!
//! 1. Execution profiles: every literal `fetch_and_run_with_runner` call site in ACFS must match
//!    the built-in profile table (interpreter, args, env).
//! 2. Base image packages: the apt list in `docker/Dockerfile.base` must be a superset of the
//!    packages ACFS installs in `scripts/generated/install_base.sh`.

use automated_flywheel_setup_checker::checksums::{profile_drift, scan_acfs_repo};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn acfs_repo() -> Option<PathBuf> {
    let p = std::env::var("AFSC_ACFS_REPO").ok().map(PathBuf::from)?;
    if p.join("scripts/lib/security.sh").exists() {
        Some(p)
    } else {
        eprintln!("SKIP: AFSC_ACFS_REPO={} has no scripts/lib/security.sh", p.display());
        None
    }
}

#[test]
fn built_in_profiles_match_acfs_call_sites() {
    let Some(repo) = acfs_repo() else {
        eprintln!("SKIP: set AFSC_ACFS_REPO to run the ACFS drift checks");
        return;
    };
    let scan = scan_acfs_repo(&repo).expect("scan ACFS repo");
    assert!(!scan.known_installers.is_empty(), "KNOWN_INSTALLERS parsed");
    assert!(!scan.call_sites.is_empty(), "literal call sites found in {:?}", scan.scanned_files);
    let drift = profile_drift(&scan.call_sites);
    assert!(
        drift.is_empty(),
        "built-in ACFS profile table drifted from the checkout:\n{}",
        drift
            .iter()
            .map(|d| format!("  {} {}: ACFS={:?} profile={:?} ({}:{})", d.name, d.field, d.acfs, d.profile, d.file, d.line))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Packages named on an `apt-get … install -y …` line.
fn apt_packages(text: &str) -> BTreeSet<String> {
    let mut pkgs = BTreeSet::new();
    for line in text.lines() {
        let Some(idx) = line.find("install -y") else { continue };
        if line.trim_start().starts_with('#') || line.contains("dry-run") || line.contains("log_") {
            continue;
        }
        let rest = &line[idx + "install -y".len()..];
        for tok in rest.split_whitespace() {
            if tok.starts_with('-') || tok.starts_with('$') || tok.starts_with('"') || tok.contains('(') {
                continue;
            }
            if tok == "\\" || tok == "&&" || tok == "||" {
                break;
            }
            pkgs.insert(tok.trim_end_matches('\\').to_string());
        }
    }
    pkgs
}

/// Packages in the Dockerfile's multi-line `apt-get install` RUN block.
fn dockerfile_packages(text: &str) -> BTreeSet<String> {
    let mut pkgs = BTreeSet::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.contains("apt-get install") {
            inside = true;
            continue;
        }
        if inside {
            if t.starts_with("&&") || t.is_empty() {
                inside = false;
                continue;
            }
            for tok in t.trim_end_matches('\\').split_whitespace() {
                pkgs.insert(tok.to_string());
            }
        }
    }
    pkgs
}

#[test]
fn base_image_installs_a_superset_of_acfs_base_packages() {
    let Some(repo) = acfs_repo() else {
        eprintln!("SKIP: set AFSC_ACFS_REPO to run the ACFS drift checks");
        return;
    };
    let base_script = repo.join("scripts/generated/install_base.sh");
    let text = std::fs::read_to_string(&base_script)
        .unwrap_or_else(|e| panic!("read {}: {e}", base_script.display()));
    let mut acfs = apt_packages(&text);
    assert!(acfs.contains("curl") && acfs.contains("build-essential"), "parsed ACFS packages: {acfs:?}");
    // install.sh's "Installing required apt packages" step runs before every tool installer and
    // is where verification tools such as minisign (mcp_agent_mail, caam) come from.
    let installer = repo.join("install.sh");
    let text = std::fs::read_to_string(&installer).unwrap_or_else(|e| panic!("read {}: {e}", installer.display()));
    let required: BTreeSet<String> = text
        .lines()
        .filter(|l| l.contains("Installing required apt packages") && l.contains("install -y"))
        .flat_map(apt_packages)
        .collect();
    assert!(required.contains("minisign"), "parsed ACFS required packages: {required:?}");
    acfs.extend(required);

    let dockerfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docker/Dockerfile.base");
    let ours = dockerfile_packages(&std::fs::read_to_string(&dockerfile).unwrap());
    let missing: Vec<&String> = acfs.iter().filter(|p| !ours.contains(*p)).collect();
    assert!(
        missing.is_empty(),
        "docker/Dockerfile.base lacks packages ACFS installs in install_base.sh / install.sh (required apt packages): {missing:?} (ACFS list: {acfs:?})"
    );
}

#[test]
fn package_parsers_handle_the_expected_shapes() {
    let script = "apt-get -o DPkg::Lock::Timeout=120 install -y curl git ca-certificates unzip tar xz-utils jq build-essential gnupg lsb-release\n        log_info \"dry-run: install: apt-get install -y curl (root)\"\n";
    let pkgs = apt_packages(script);
    assert_eq!(pkgs.len(), 10, "{pkgs:?}");
    assert!(pkgs.contains("lsb-release"));
    let docker = "RUN apt-get update -qq && \\\n    apt-get install -y -qq \\\n        curl ca-certificates git \\\n        build-essential sudo gnupg lsb-release \\\n    && rm -rf /var/lib/apt/lists/*\n";
    let ours = dockerfile_packages(docker);
    assert!(ours.contains("curl") && ours.contains("lsb-release") && !ours.contains("&&"), "{ours:?}");
}
