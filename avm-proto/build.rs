use std::{env, fs, path::PathBuf};

/// Compile `proto/avm_service.proto` into `$OUT_DIR/avm.v1.rs`.
///
/// `tonic-build` shells out to `protoc`. On machines without it we emit an
/// empty module instead of failing the build, so the workspace always
/// compiles; set `AVM_PROTO_STRICT=1` to turn that fallback into a hard error.
fn main() {
    let proto = PathBuf::from("../proto/avm_service.proto");
    println!("cargo:rerun-if-changed=../proto/avm_service.proto");
    println!("cargo:rerun-if-env-changed=AVM_PROTO_STRICT");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));

    let result = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[PathBuf::from("../proto")]);

    match result {
        Ok(()) => {}
        Err(err) => {
            if env::var("AVM_PROTO_STRICT").as_deref() == Ok("1") {
                panic!("protobuf codegen failed (AVM_PROTO_STRICT=1): {err}");
            }
            println!(
                "cargo:warning=avm-proto: protoc codegen unavailable ({err}); \
                 emitting empty stub. Install protobuf-compiler for generated gRPC types."
            );
            let stub = out_dir.join("avm.v1.rs");
            fs::write(&stub, "// protoc unavailable at build time; stub module.\n")
                .expect("failed to write proto stub");
        }
    }
}
