//! Headset UI: software-rendered panels shown on OpenXR quad layers.

pub mod browser;
pub mod canvas;
pub mod captions;
pub mod controls;
pub mod focus;
pub mod form;
pub mod navigator;
pub mod settings;

/// Saves a canvas as PNG (UI previews and tests).
pub fn save_png(canvas: &canvas::Canvas, path: &std::path::Path) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, canvas.width, canvas.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&canvas.pixels)?;
    Ok(())
}
