fn main() {
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
