fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use a vendored protoc so this builds without requiring protoc to be
    // installed on the machine (dev box, CI, or the VPN node itself).
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        // Tell prost where the common packages live in Rust-module space,
        // so the generated command.rs doesn't emit broken super::super::super
        // paths for cross-package references.
        .extern_path(".xray.common.protocol", "crate::core::xray_api::pb::protocol")
        .extern_path(".xray.common.serial",   "crate::core::xray_api::pb::serial")
        .compile_protos(
            &[
                "proto/app/proxyman/command/command.proto",
                "proto/app/stats/command/command.proto",
                "proto/proxy/vless/account.proto",
                "proto/proxy/vmess/account.proto",
                "proto/proxy/trojan/account.proto",
                "proto/proxy/shadowsocks/account.proto",
            ],
            &["proto"],
        )?;

    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
