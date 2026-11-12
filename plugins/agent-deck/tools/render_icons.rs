// cargo run -p corvid-agent-deck --example render-icons
use resvg::{tiny_skia, usvg};
use std::{fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (directory, name) in [
        (root.join("assets/icons"), "corvid"),
        (root.join("plugins/agent-deck/assets"), "tray"),
        (root.join("plugins/agent-deck/assets"), "tray-attention"),
    ] {
        let tree = usvg::Tree::from_data(
            &fs::read(directory.join(format!("{name}.svg")))?,
            &usvg::Options::default(),
        )?;
        let sizes = [16_u32, 20, 24, 32, 48, 64, 128, 256];
        let mut images = Vec::new();
        for size in sizes {
            let mut image = tiny_skia::Pixmap::new(size, size).unwrap();
            let scale = size as f32 / tree.size().width();
            resvg::render(
                &tree,
                tiny_skia::Transform::from_scale(scale, scale),
                &mut image.as_mut(),
            );
            let png = image.encode_png()?;
            if size == 256 {
                fs::write(directory.join(format!("{name}.png")), &png)?;
            }
            images.push(png);
        }
        let mut ico = vec![0, 0, 1, 0, sizes.len() as u8, 0];
        let mut offset = 6 + 16 * sizes.len() as u32;
        for (size, image) in sizes.into_iter().zip(&images) {
            ico.extend_from_slice(&[size as u8, size as u8, 0, 0, 1, 0, 32, 0]);
            ico.extend_from_slice(&(image.len() as u32).to_le_bytes());
            ico.extend_from_slice(&offset.to_le_bytes());
            offset += image.len() as u32;
        }
        for image in images {
            ico.extend(image);
        }
        fs::write(directory.join(format!("{name}.ico")), ico)?;
    }
    Ok(())
}
