//! System clipboard as the programs in a pane see it. Copy, cut, paste and
//! select-all are the programs' own shortcuts; delta only gives them this.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paste {
	Text(String),
	/// PNG bytes of a clipboard image.
	Image(Vec<u8>),
}

pub fn text() -> Option<String> {
	for _ in 0..5 {
		if let Ok(mut clipboard) = arboard::Clipboard::new() {
			if let Ok(text) = clipboard.get_text() {
				if !text.is_empty() {
					return Some(text);
				}
			}
			return None;
		}
		std::thread::sleep(Duration::from_millis(10));
	}
	None
}

fn image() -> Option<Vec<u8>> {
	for _ in 0..3 {
		let Ok(mut clipboard) = arboard::Clipboard::new() else {
			continue;
		};
		let Ok(image) = clipboard.get_image() else {
			return None;
		};
		let width = image.width as u32;
		let height = image.height as u32;
		return encode_png(width, height, &image.bytes);
	}
	None
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
	let mut png = Vec::new();
	{
		let mut encoder = png::Encoder::new(&mut png, width, height);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		let mut writer = encoder.write_header().ok()?;
		writer.write_image_data(rgba).ok()?;
	}
	Some(png)
}

pub fn paste() -> Option<Paste> {
	if let Some(text) = text() {
		return Some(Paste::Text(text));
	}
	image().map(Paste::Image)
}

/// Clipboard content stays content: text is text, an image keeps its bytes.
pub fn set(text: &str) {
	for _ in 0..5 {
		if let Ok(mut clipboard) = arboard::Clipboard::new() {
			if clipboard.set_text(text.to_string()).is_ok() {
				return;
			}
		}
		std::thread::sleep(Duration::from_millis(10));
	}
}

/// Write PNG bytes to a fresh file in the temporary directory and return its
/// path, so a pasted image can appear in text as `![](path)`.
pub fn save_image(png: &[u8]) -> Option<PathBuf> {
	let stamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis())
		.unwrap_or(0);
	let path = std::env::temp_dir().join(format!("delta-paste-{stamp}.png"));
	std::fs::write(&path, png).ok()?;
	Some(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn image_is_encoded_as_png_bytes() {
		let image = encode_png(1, 1, &[255, 0, 0, 255]).unwrap();
		assert_eq!(&image[..8], b"\x89PNG\r\n\x1a\n");
		let decoder = png::Decoder::new(image.as_slice());
		let mut reader = decoder.read_info().unwrap();
		let mut out = vec![0; reader.output_buffer_size()];
		let info = reader.next_frame(&mut out).unwrap();
		assert_eq!((info.width, info.height), (1, 1));
		assert_eq!(&out[..info.buffer_size()], &[255, 0, 0, 255]);
	}

	#[test]
	fn saved_image_keeps_the_png_bytes() {
		let image = encode_png(1, 1, &[0, 255, 0, 255]).unwrap();
		let path = save_image(&image).unwrap();
		let saved = std::fs::read(&path).unwrap();
		assert_eq!(saved, image);
		let _ = std::fs::remove_file(&path);
	}
}
