use prost::Message;

fn main() {
    // Not protoc or buf through connectrpc-build: it needs one of them on PATH, and this container and the CI image have neither.
    let descriptors = protox::compile(["proto/workspace.proto"], ["proto"]).expect("compile proto");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let descriptor_path = std::path::Path::new(&out_dir).join("workspace.descriptor");
    std::fs::write(&descriptor_path, descriptors.encode_to_vec()).expect("write descriptor");
    connectrpc_build::Config::new()
        .files(&["workspace.proto"])
        .descriptor_set(&descriptor_path)
        .include_file("_connectrpc.rs")
        // Not the default directives: they watch the descriptor that this script rewrites, so every build would run it again.
        .emit_rerun_directives(false)
        .compile()
        .expect("generate connectrpc code");
    println!("cargo:rerun-if-changed=proto");
}
