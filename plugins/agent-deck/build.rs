fn main() {
    println!("cargo:rerun-if-changed=src/platform/macos.m");
    if std::env::var_os("CARGO_FEATURE_UI").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos")
    {
        cc::Build::new()
            .file("src/platform/macos.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .compile("corvid_macos");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=UserNotifications");
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
