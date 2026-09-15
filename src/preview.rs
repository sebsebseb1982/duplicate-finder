//! Aperçu du fichier sélectionné : image, texte ou vidage hexadécimal.

use std::io::Read;
use std::path::{Path, PathBuf};

use eframe::egui;

const TEXT_LIMIT: usize = 64 * 1024;
const HEX_LIMIT: usize = 1024;

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "tif", "tiff"];

pub enum PreviewKind {
    Image(String),
    Text { content: String, truncated: bool },
    Binary(String),
    Error(String),
}

pub struct Preview {
    pub path: PathBuf,
    pub kind: PreviewKind,
}

impl Preview {
    pub fn load(path: &Path) -> Self {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let kind = if IMAGE_EXT.contains(&ext.as_str()) {
            PreviewKind::Image(format!("file://{}", path.display()))
        } else {
            match read_head(path, TEXT_LIMIT + 1) {
                Ok(bytes) => classify(bytes),
                Err(e) => PreviewKind::Error(e.to_string()),
            }
        };
        Self { path: path.to_path_buf(), kind }
    }

    /// Libère la texture de l'image affichée.
    pub fn release(&self, ctx: &egui::Context) {
        if let PreviewKind::Image(uri) = &self.kind {
            ctx.forget_image(uri);
        }
    }

    pub fn show(&self, ui: &mut egui::Ui) {
        match &self.kind {
            PreviewKind::Image(uri) => {
                ui.add(egui::Image::new(uri.as_str()).max_width(ui.available_width()).max_height(400.0).shrink_to_fit());
            }
            PreviewKind::Text { content, truncated } => {
                egui::ScrollArea::both().max_height(400.0).id_salt("preview-text").show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(content).monospace()).extend());
                    if *truncated {
                        ui.weak("… (aperçu tronqué)");
                    }
                });
            }
            PreviewKind::Binary(dump) => {
                ui.weak("Fichier binaire — premiers octets :");
                egui::ScrollArea::both().max_height(400.0).id_salt("preview-hex").show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(dump).monospace()).extend());
                });
            }
            PreviewKind::Error(e) => {
                ui.colored_label(egui::Color32::from_rgb(220, 120, 0), format!("Aperçu impossible : {e}"));
            }
        }
    }
}

fn read_head(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit);
    std::fs::File::open(path)?.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn classify(mut bytes: Vec<u8>) -> PreviewKind {
    let truncated = bytes.len() > TEXT_LIMIT;
    bytes.truncate(TEXT_LIMIT);
    if !bytes.contains(&0) {
        let text = String::from_utf8_lossy(&bytes);
        let invalid = text.chars().filter(|&c| c == char::REPLACEMENT_CHARACTER).count();
        // Tolère un caractère coupé en fin de tampon ou quelques octets Latin-1.
        if invalid * 100 <= text.chars().count().max(1) {
            return PreviewKind::Text { content: text.into_owned(), truncated };
        }
    }
    PreviewKind::Binary(hex_dump(&bytes[..bytes.len().min(HEX_LIMIT)]))
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for j in 0..16 {
            match chunk.get(j) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
        }
        out.push(' ');
        out.extend(chunk.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }));
        out.push('\n');
    }
    out
}
