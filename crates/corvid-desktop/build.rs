fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "macos" {
        println!("cargo:rerun-if-changed=src/macos/window.m");
        println!("cargo:rerun-if-changed=src/workspace_view/deck_focus.m");
        cc::Build::new()
            .file("src/macos/window.m")
            .file("src/workspace_view/deck_focus.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .compile("corvid_desktop_macos");
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
    if os == "windows" {
        embed_resource::compile_for(
            "../../assets/icons/app.rc",
            ["corvid-desktop"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("compile Corvid Desktop icon");
    }
}
