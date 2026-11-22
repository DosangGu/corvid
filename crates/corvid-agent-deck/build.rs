fn main() {
    println!("cargo:rerun-if-changed=src/unix_lifecycle/macos.m");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        cc::Build::new()
            .file("src/unix_lifecycle/macos.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .compile("corvid_agent_deck_lifecycle");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }
}
