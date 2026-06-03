use std::env;
use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto_file = proto_dir.join("rgraph.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_dir])
        .expect("failed to compile protobuf schemas");
}
