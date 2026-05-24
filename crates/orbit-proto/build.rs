fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tonic 0.14 moved prost integration to tonic-prost-build.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../../proto/etl.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/etl.proto");
    Ok(())
}
