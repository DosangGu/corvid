fn main() {
    println!("cargo:rerun-if-changed=src/platform/macos.m");
    println!("cargo:rerun-if-changed=src/shell/window/macos.m");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        cc::Build::new()
            .file("src/platform/macos.m")
            .file("src/shell/window/macos.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .compile("corvid_agent_deck_ui");
        println!("cargo:rustc-link-lib=framework=UserNotifications");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }

    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        embed_resource::compile_for("assets/app.rc", ["corvid-agent-deck"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Agent Deck icons");
    }
}
