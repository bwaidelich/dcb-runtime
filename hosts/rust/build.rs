//! Generates the Axon Server client from its protocol definition, with
//! protox – a protobuf compiler in Rust – so no `protoc` is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "axonserver")]
    {
        let descriptors = protox::compile(["dcb.proto"], ["proto/axonserver"])?;
        tonic_prost_build::configure().build_server(false).compile_fds(descriptors)?;
        println!("cargo:rerun-if-changed=proto/axonserver");
    }
    Ok(())
}
