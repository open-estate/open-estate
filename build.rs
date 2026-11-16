fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = [
        "proto/openestate/extension/v1/domains.proto",
        "proto/openestate/extension/v1/extension.proto",
        "proto/openestate/extension/v1/aggregation.proto",
    ];
    for p in &proto_files {
        println!("cargo:rerun-if-changed={p}");
    }

    let file_descriptors = protox::compile(proto_files, ["proto"])?;
    let mut config = prost_build::Config::new();
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config.type_attribute(".", "#[serde(rename_all = \"camelCase\")]");
    config.compile_fds(file_descriptors)?;
    Ok(())
}
