fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use a vendored protoc so this builds without requiring protoc to be
    // installed on the machine (dev box, CI, or the VPN node itself).
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    // Compile the shared "common" messages (User, TypedMessage) on their
    // own first, with no extern_path remapping. This is what actually
    // produces xray.common.protocol.rs / xray.common.serial.rs, which
    // src/core/xray_api.rs's `pb::protocol` / `pb::serial` modules include
    // directly via `tonic::include_proto!`. These two packages must NOT be
    // extern_path'd in this call - extern_path tells prost "don't generate
    // this package, it lives elsewhere", and pointing it at itself is
    // circular: the file it's supposed to come from is the one that would
    // never get written.
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .compile_protos(
            &[
                "proto/common/protocol/user.proto",
                "proto/common/serial/typed_message.proto",
            ],
            &["proto"],
        )?;

    // Now compile everything else, telling prost that any reference to
    // `.xray.common.protocol` / `.xray.common.serial` should point at the
    // sibling modules generated above (`pb::protocol` / `pb::serial`)
    // instead of the broken `super::super::super` relative paths it would
    // otherwise emit for packages compiled outside that nesting.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
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
