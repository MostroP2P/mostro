use std::process::Command;
fn main() {
    // note: add error checking yourself.
    println!("cargo:rerun-if-changed=.git/refs/head/main");
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let git_hash = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
}
