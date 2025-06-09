use std::process::Command;
fn main() {
    // Compile protobuf definitions
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&["proto/admin.proto"], &["proto"])
        .unwrap_or_else(|e| panic!("Failed to compile protos {:?}", e));

    // note: add error checking yourself.
    println!("cargo:rerun-if-changed=.git/refs/head/main");
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let git_hash = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
}
