use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

struct FontSource {
    key: &'static str,
    file: &'static str,
    index: u32,
}

/// System fonts used as fallbacks (tried in order per character).
const WIN_FONTS: &[FontSource] = &[
    FontSource {
        key: "segoe_ui",
        file: "segoeui.ttf",
        index: 0,
    },
    FontSource {
        key: "msyh",
        file: "msyh.ttc",
        index: 0,
    },
    FontSource {
        key: "yugoth",
        file: "YuGothR.ttc",
        index: 0,
    },
    FontSource {
        key: "leelawadee",
        file: "LeelawUI.ttf",
        index: 0,
    },
    FontSource {
        key: "simsun",
        file: "simsun.ttc",
        index: 0,
    },
];

fn windows_fonts_dir() -> Option<PathBuf> {
    std::env::var_os("WINDIR").map(|d| PathBuf::from(d).join("Fonts"))
}

fn load_font(path: &PathBuf, index: u32) -> Option<Arc<FontData>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 4 {
        return None;
    }
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    Some(Arc::new(data))
}

pub fn setup_fonts(ctx: &egui::Context) {
    ctx.input_mut(|i| i.max_texture_side = 8192);

    let mut fonts = FontDefinitions::default();
    let mut loaded: Vec<&'static str> = Vec::new();

    if let Some(dir) = windows_fonts_dir() {
        for src in WIN_FONTS {
            let path = dir.join(src.file);
            if let Some(data) = load_font(&path, src.index) {
                fonts.font_data.insert(src.key.to_owned(), data);
                loaded.push(src.key);
            }
        }
    }

    if loaded.is_empty() {
        const LINUX_FONTS: &[(&str, &str)] = &[
            ("dejavu", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            (
                "noto",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            ),
            (
                "liberation",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            ),
            (
                "noto_cjk",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            ),
        ];
        for (key, path) in LINUX_FONTS {
            let path = PathBuf::from(path);
            if let Some(data) = load_font(&path, 0) {
                fonts.font_data.insert(key.to_string(), data);
                loaded.push(key);
            }
        }
    }

    if !loaded.is_empty() {
        let family = fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default();
        family.clear();
        for key in &loaded {
            family.push((*key).to_owned());
        }
    }

    ctx.set_fonts(fonts);
}