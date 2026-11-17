use std::env;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ext_proto_files = [
        "proto/openestate/extension/v1/domains.proto",
        "proto/openestate/extension/v1/extension.proto",
        "proto/openestate/extension/v1/aggregation.proto",
        "proto/openestate/extension/v1/metering.proto",
        "proto/openestate/scheduler/v1/scheduler.proto",
        "proto/openestate/shared/v1/shared_data.proto",
    ];
    let db_proto_files = [
        "proto/openestate/db/v1/enums.proto",
        "proto/openestate/db/v1/listing.proto",
        "proto/openestate/db/v1/factors.proto",
        "proto/openestate/db/v1/report.proto",
        "proto/openestate/db/v1/subscription.proto",
    ];
    for p in ext_proto_files.iter().chain(db_proto_files.iter()) {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=proto");

    let file_descriptors = protox::compile(ext_proto_files, ["proto"])?;
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("extension.descriptor.bin");
    let descriptor_bytes = protox::prost::Message::encode_to_vec(&file_descriptors);
    fs::write(&descriptor_path, descriptor_bytes)?;

    let mut ext_config = prost_build::Config::new();
    ext_config.file_descriptor_set_path(&descriptor_path);
    ext_config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    ext_config.type_attribute(".", "#[serde(rename_all = \"camelCase\")]");
    ext_config.message_attribute(".", "#[serde(default)]");
    ext_config.compile_fds(file_descriptors)?;

    let db_fds = protox::compile(db_proto_files, ["proto"])?;
    let mut db_config = prost_build::Config::new();
    db_config.extern_path(".openestate.extension.v1", "crate::extensions::proto");
    db_config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    db_config.type_attribute(".", "#[serde(rename_all = \"camelCase\")]");
    db_config.compile_fds(db_fds)?;

    Ok(())
}
