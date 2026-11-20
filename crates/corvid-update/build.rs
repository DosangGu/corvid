fn main() {
    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        embed_resource::compile_for(
            "assets/app.rc",
            [
                "corvid-launch",
                "corvid-desktop-launch",
                "corvid-agent-deck-launch",
            ],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("compile Corvid launcher icons");
    }
}
