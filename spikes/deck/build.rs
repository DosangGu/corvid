fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons");
    if std::env::var_os("CARGO_FEATURE_DESKTOP").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows")
    {
        embed_resource::compile_for(
            "../../assets/icons/app.rc",
            ["corvid-deck"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("compile Corvid Deck icon");
    }
}
