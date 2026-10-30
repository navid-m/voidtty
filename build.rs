use std::{path::Path, process::Command};

fn main() {
    emit_git_commit();

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("embedding Windows icon");
    }
}

/// Exposes the current git commit as `VOIDTTY_GIT_COMMIT` so `--about` can
/// report exactly which revision was built.
fn emit_git_commit() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.strip_prefix("ref: ")
    {
        let path = Path::new(".git").join(reference.trim());
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if Path::new(".git/packed-refs").exists() {
        println!("cargo:rerun-if-changed=.git/packed-refs");
    }

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hash| !hash.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=VOIDTTY_GIT_COMMIT={commit}");
}
