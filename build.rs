use std::process::Command;

fn main() {
    // .cargo/config.toml already sets PROTOC (via the protoc-wrapper script)
    // for every build-script process, including fedimint-tonic-lnd's.
    // We keep protoc-bin-vendored here as a fallback so that this build script
    // also works when the config isn't in effect (e.g. when building the crate
    // as a path dependency outside of this workspace).
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()
            .expect("protoc-bin-vendored: could not determine protoc path");
        std::env::set_var("PROTOC", &protoc);
    }

    // Compile protobuf definitions for the admin RPC service.
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&["proto/admin.proto"], &["proto"])
        .unwrap_or_else(|e| panic!("Failed to compile protos {:?}", e));

    println!("cargo:rerun-if-changed=.git/refs/head/main");
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let git_hash = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
}
