use std::env;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = [
        "proto/openestate/extension/v1/domains.proto",
        "proto/openestate/extension/v1/extension.proto",
        "proto/openestate/extension/v1/aggregation.proto",
        "proto/openestate/extension/v1/metering.proto",
        "proto/openestate/scheduler/v1/scheduler.proto",
        "proto/openestate/shared/v1/shared_data.proto",
    ];
    for p in &proto_files {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=proto");

    let file_descriptors = protox::compile(proto_files, ["proto"])?;
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("extension.descriptor.bin");
    let descriptor_bytes = protox::prost::Message::encode_to_vec(&file_descriptors);
    fs::write(&descriptor_path, descriptor_bytes)?;

    let mut config = prost_build::Config::new();
    config.file_descriptor_set_path(&descriptor_path);
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config.type_attribute(".", "#[serde(rename_all = \"camelCase\")]");
    config.message_attribute(".", "#[serde(default)]");
    config.compile_fds(file_descriptors)?;
    Ok(())
}
