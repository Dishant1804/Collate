//! Records the commit Collate is built from, so an app that updates from source can tell
//! whether the newest commit is the one it already runs.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = Command::new("git").args(["rev-parse", "HEAD"]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    println!("cargo:rustc-env=COLLATE_COMMIT={commit}");
    // A path that doesn't exist would rerun this every build, so name only those that do.
    for p in [".git/HEAD", ".git/refs/heads", ".git/packed-refs"] {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
