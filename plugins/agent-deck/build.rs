fn main() {
    println!("cargo:rerun-if-changed=src/platform/macos.m");
    println!("cargo:rerun-if-changed=src/shell/window/macos.m");
    println!("cargo:rerun-if-changed=src/unix_lifecycle/macos.m");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        let mut native = cc::Build::new();
        native
            .file("src/unix_lifecycle/macos.m")
            .flag("-fobjc-arc")
            .flag("-fblocks");
        if std::env::var_os("CARGO_FEATURE_UI").is_some() {
            native
                .file("src/platform/macos.m")
                .file("src/shell/window/macos.m");
            println!("cargo:rustc-link-lib=framework=UserNotifications");
        }
        native.compile("corvid_macos");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }

    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=../../assets/icons");
    if std::env::var_os("CARGO_FEATURE_UI").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows")
    {
        embed_resource::compile_for("assets/app.rc", ["corvid-agent-deck"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Agent Deck icons");
    }
}
