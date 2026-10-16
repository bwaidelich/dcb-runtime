//! Checks that the runtime component, which the host embeds, is built,
//! and generates the Axon Server client from its protocol definition,
//! with protox – a protobuf compiler in Rust – so no `protoc` is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wasm = "../../build/dcb-runtime.wasm";
    println!("cargo:rerun-if-changed={wasm}");
    if !std::path::Path::new(wasm).exists() {
        return Err("build/dcb-runtime.wasm is missing: run `npm install && npm run build` in the repository root first".into());
    }
    #[cfg(feature = "axonserver")]
    {
        let descriptors = protox::compile(["dcb.proto"], ["proto/axonserver"])?;
        tonic_prost_build::configure().build_server(false).compile_fds(descriptors)?;
        println!("cargo:rerun-if-changed=proto/axonserver");
    }
    Ok(())
}
