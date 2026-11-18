use std::collections::BTreeSet;

use prost::Message;
use prost_types::{FileDescriptorSet, field_descriptor_proto::Type};

fn main() {
    // Not protoc or buf through connectrpc-build: it needs one of them on PATH, and this container and the CI image have neither.
    let descriptors = protox::compile(["proto/workspace.proto"], ["proto"]).expect("compile proto");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let descriptor_path = std::path::Path::new(&out_dir).join("workspace.descriptor");
    std::fs::write(&descriptor_path, descriptors.encode_to_vec()).expect("write descriptor");
    // Not the buffa default: it ignores an unknown JSON field, so a misspelled at_least would read without the freshness that the client asked for.
    let mut buffa = connectrpc_build::CodeGenConfig::default();
    buffa.generate_json = true;
    buffa.message_attributes = request_messages(&descriptors)
        .into_iter()
        .map(|name| (name, "#[serde(deny_unknown_fields)]".to_owned()))
        .collect();
    connectrpc_build::Config::new()
        .files(&["workspace.proto"])
        .descriptor_set(&descriptor_path)
        .include_file("_connectrpc.rs")
        .buffa_config(buffa)
        // Not the default directives: they watch the descriptor that this script rewrites, so every build would run it again.
        .emit_rerun_directives(false)
        .compile()
        .expect("generate connectrpc code");
    println!("cargo:rerun-if-changed=proto");
}

fn request_messages(descriptors: &FileDescriptorSet) -> BTreeSet<String> {
    let messages: Vec<_> = descriptors
        .file
        .iter()
        .flat_map(|file| {
            let package = file.package().to_owned();
            file.message_type
                .iter()
                .map(move |message| (format!(".{package}.{}", message.name()), message))
        })
        .collect();
    let mut pending: Vec<String> = descriptors
        .file
        .iter()
        .flat_map(|file| &file.service)
        .flat_map(|service| &service.method)
        .map(|method| method.input_type().to_owned())
        .collect();
    let mut requests = BTreeSet::new();
    while let Some(name) = pending.pop() {
        let Some((_, message)) = messages.iter().find(|(candidate, _)| *candidate == name) else {
            assert!(
                name.starts_with(".google."),
                "{name} is in a request, and this script does not find it to refuse its unknown fields"
            );
            continue;
        };
        if requests.insert(name) {
            pending.extend(
                message
                    .field
                    .iter()
                    .filter(|field| field.r#type() == Type::Message)
                    .map(|field| field.type_name().to_owned()),
            );
        }
    }
    requests
}
