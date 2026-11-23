use std::env;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=assets");
    if env::var("CARGO_CFG_TARGET_OS")? == "windows" {
        embed_resource::compile_for("assets/app.rc", ["corvid"], embed_resource::NONE)
            .manifest_required()?;
    }
    Ok(())
}
