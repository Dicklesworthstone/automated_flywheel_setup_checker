//! Embed build provenance for `--version`: git sha (+dirty), build date, rustc version.
//! Every value has a fallback so builds outside a git checkout (cargo install, tarballs) work.

use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let sha = run("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = run("git", &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let git = if dirty { format!("{sha}-dirty") } else { sha };
    let date = run("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".into());
    let rustc = std::env::var("RUSTC")
        .ok()
        .and_then(|rc| run(&rc, &["-V"]))
        .or_else(|| run("rustc", &["-V"]))
        .unwrap_or_else(|| "rustc unknown".into());

    println!("cargo:rustc-env=AFSC_GIT_SHA={git}");
    println!("cargo:rustc-env=AFSC_BUILD_DATE={date}");
    println!("cargo:rustc-env=AFSC_RUSTC={rustc}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=build.rs");
}
