//! Image and text preview support.
//!
//! Detects the terminal's graphics protocol (kitty / sixel / iterm2),
//! provides backends that emit the right escape sequences, and classifies
//! files into text / image / unsupported for the viewer.

use std::env;
use std::io::{Cursor, Write};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::ImageFormat;

use crate::config::ImagePreviewMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsProtocol {
    Kitty,
    Sixel,
    Iterm2,
    None,
}

/// Inspect environment variables to figure out what the host terminal supports.
pub fn detect(prefer: ImagePreviewMode) -> GraphicsProtocol {
    if prefer == ImagePreviewMode::None {
        return GraphicsProtocol::None;
    }

    match prefer {
        ImagePreviewMode::Kitty => return GraphicsProtocol::Kitty,
        ImagePreviewMode::Sixel => return GraphicsProtocol::Sixel,
        ImagePreviewMode::Iterm2 => return GraphicsProtocol::Iterm2,
        _ => {}
    }

    let term = env::var("TERM").unwrap_or_default();
    let term_program = env::var("TERM_PROGRAM").unwrap_or_default();

    if term.contains("kitty") || env::var("KITTY_WINDOW_ID").is_ok() {
        return GraphicsProtocol::Kitty;
    }
    if term_program == "WezTerm" {
        return GraphicsProtocol::Kitty;
    }
    if term_program == "ghostty" || env::var("GHOSTTY_RESOURCES_DIR").is_ok() {
        return GraphicsProtocol::Kitty;
    }
    if term_program == "iTerm.app" {
        return GraphicsProtocol::Iterm2;
    }
    // Konsole has supported sixel since ~22.04 and sets KONSOLE_VERSION.
    if env::var("KONSOLE_VERSION").is_ok() {
        return GraphicsProtocol::Sixel;
    }
    // mlterm sets MLTERM; xterm with sixel sets XTERM_VERSION.
    if env::var("MLTERM").is_ok() {
        return GraphicsProtocol::Sixel;
    }
    if term.contains("xterm") && env::var("XTERM_VERSION").is_ok() {
        return GraphicsProtocol::Sixel;
    }
    GraphicsProtocol::None
}

/// What every graphics-protocol implementation must provide.
pub trait PreviewBackend {
    /// Render `image_bytes` (PNG/JPEG/etc.) into the area at terminal cell
    /// `(col, row)` with size `(cols, rows)`. Returns the raw byte sequence to
    /// write to the terminal. `(col, row)` are 0-indexed.
    fn render(&self, image_bytes: &[u8], col: u16, row: u16, cols: u16, rows: u16) -> Vec<u8>;
}

/// Approximate cell-to-pixel ratio used when the terminal doesn't report its
/// real cell size. Different terminals have different cell sizes; the runtime
/// query ([`cell_pixels`]) gets us the exact values where supported.
const FALLBACK_PX_PER_COL: u32 = 10;
const FALLBACK_PX_PER_ROW: u32 = 20;

/// Maximum width or height (in pixels) accepted after decoding a remote image.
///
/// A highly-compressed image (e.g. a 10 MB PNG that declares 16 000 × 16 000
/// pixels) would otherwise allocate hundreds of megabytes of RGBA data inside
/// `image::load_from_memory` and then trigger an expensive Lanczos3 resample.
/// 4096 px per side is far beyond any terminal preview panel and keeps the
/// worst-case post-decode buffer under ~64 MiB.
const MAX_IMAGE_DIMENSION: u32 = 4096;

/// Query the terminal for cell pixel dimensions.
///
/// Returns `(px_per_col, px_per_row)`. Uses crossterm's `window_size()` which
/// goes through `TIOCGWINSZ` on Unix. Many terminals — Konsole, kitty, iTerm2,
/// xterm with `-tn xterm-direct`, etc. — populate `ws_xpixel` and `ws_ypixel`
/// correctly. When the report comes back as zero (some old terminals, the
/// Windows console host) we fall back to a reasonable default.
pub fn cell_pixels() -> (u32, u32) {
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 {
            return (
                (ws.width as u32 / ws.columns as u32).max(1),
                (ws.height as u32 / ws.rows as u32).max(1),
            );
        }
    }
    (FALLBACK_PX_PER_COL, FALLBACK_PX_PER_ROW)
}

/// Result of fitting an image into a cell-bounded panel: pre-scaled RGBA
/// bytes plus the centered display position and cell extent.
struct ScaledForCells {
    rgba: Vec<u8>,
    width_px: u32,
    height_px: u32,
    /// Centered top-left position, in 0-indexed terminal cells.
    display_col: u16,
    display_row: u16,
    /// How many cells the displayed image will occupy.
    cells_w: u16,
    cells_h: u16,
}

/// Decode the image, downscale to fit the panel area while preserving the
/// aspect ratio, and compute the centered display offset.
///
/// Never upscales: if the image is smaller than the panel, it stays at its
/// native pixel size and is centered.
fn scale_for_cells(
    image_bytes: &[u8],
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
) -> Option<ScaledForCells> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let (px_per_col, px_per_row) = cell_pixels();
    let img = image::load_from_memory(image_bytes).ok()?;

    // Reject images that exceed the dimension cap. We cannot check dimensions
    // before load_from_memory (no lazy-decode API), but we can fail fast here
    // to avoid the expensive Lanczos3 resample and subsequent allocations.
    if img.width() > MAX_IMAGE_DIMENSION || img.height() > MAX_IMAGE_DIMENSION {
        return None;
    }

    let (iw, ih) = (img.width().max(1), img.height().max(1));

    let panel_w_px = u32::from(cols).saturating_mul(px_per_col).max(1);
    let panel_h_px = u32::from(rows).saturating_mul(px_per_row).max(1);

    // Aspect-preserving scale factor (downscale only).
    let scale_w = panel_w_px as f64 / iw as f64;
    let scale_h = panel_h_px as f64 / ih as f64;
    let scale = scale_w.min(scale_h).min(1.0);

    let scaled_w = ((iw as f64 * scale) as u32).max(1);
    let scaled_h = ((ih as f64 * scale) as u32).max(1);

    let resized = if scaled_w == iw && scaled_h == ih {
        img.to_rgba8()
    } else {
        img.resize_exact(scaled_w, scaled_h, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
    };

    // Cell extent of the scaled image, rounded up so we don't truncate.
    let used_w_cells = scaled_w.div_ceil(px_per_col).max(1).min(u32::from(cols)) as u16;
    let used_h_cells = scaled_h.div_ceil(px_per_row).max(1).min(u32::from(rows)) as u16;

    let offset_x = cols.saturating_sub(used_w_cells) / 2;
    let offset_y = rows.saturating_sub(used_h_cells) / 2;

    Some(ScaledForCells {
        rgba: resized.into_raw(),
        width_px: scaled_w,
        height_px: scaled_h,
        display_col: col.saturating_add(offset_x),
        display_row: row.saturating_add(offset_y),
        cells_w: used_w_cells,
        cells_h: used_h_cells,
    })
}

/// Encode an RGBA buffer as PNG. Used by the kitty and iTerm2 backends after
/// scaling.
fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, image::ImageError> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| image::ImageError::Parameter(image::error::ParameterError::from_kind(
            image::error::ParameterErrorKind::DimensionMismatch,
        )))?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

/// Kitty graphics protocol — chunked base64 PNG transmission.
///
/// Supported by kitty, ghostty, WezTerm (with kitty mode).
pub struct KittyBackend;

impl PreviewBackend for KittyBackend {
    fn render(&self, image_bytes: &[u8], col: u16, row: u16, cols: u16, rows: u16) -> Vec<u8> {
        let scaled = match scale_for_cells(image_bytes, col, row, cols, rows) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let png = match encode_png_rgba(&scaled.rgba, scaled.width_px, scaled.height_px) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };

        let b64 = BASE64.encode(&png);
        let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
        if chunks.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(b64.len() + 256);
        let _ = write!(
            out,
            "\x1b[{};{}H",
            scaled.display_row.saturating_add(1),
            scaled.display_col.saturating_add(1)
        );

        for (i, chunk) in chunks.iter().enumerate() {
            let last = i == chunks.len() - 1;
            let m = if last { 0 } else { 1 };
            if i == 0 {
                // q=2 suppresses kitty's per-image response strings.
                let _ = write!(
                    out,
                    "\x1b_Gf=100,a=T,c={},r={},q=2,m={m};",
                    scaled.cells_w, scaled.cells_h
                );
            } else {
                let _ = write!(out, "\x1b_Gm={m};");
            }
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
        out
    }
}

/// iTerm2 inline image protocol — single OSC 1337 escape with base64 payload.
pub struct Iterm2Backend;

impl PreviewBackend for Iterm2Backend {
    fn render(&self, image_bytes: &[u8], col: u16, row: u16, cols: u16, rows: u16) -> Vec<u8> {
        let scaled = match scale_for_cells(image_bytes, col, row, cols, rows) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let png = match encode_png_rgba(&scaled.rgba, scaled.width_px, scaled.height_px) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        let b64 = BASE64.encode(&png);
        let mut out = Vec::with_capacity(b64.len() + 128);
        let _ = write!(
            out,
            "\x1b[{};{}H",
            scaled.display_row.saturating_add(1),
            scaled.display_col.saturating_add(1)
        );
        let _ = write!(
            out,
            "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{b64}\x07",
            scaled.cells_w, scaled.cells_h
        );
        out
    }
}

/// Sixel — DEC's sixel graphics protocol. Encoded via `icy_sixel`'s pure-Rust
/// implementation. Supported by Konsole (≥22.04), mlterm, foot, xterm with
/// `-ti vt340`, and others.
pub struct SixelBackend;

impl PreviewBackend for SixelBackend {
    fn render(&self, image_bytes: &[u8], col: u16, row: u16, cols: u16, rows: u16) -> Vec<u8> {
        let scaled = match scale_for_cells(image_bytes, col, row, cols, rows) {
            Some(s) => s,
            None => return Vec::new(),
        };

        let sixel_image = icy_sixel::SixelImage::from_rgba(
            scaled.rgba,
            scaled.width_px as usize,
            scaled.height_px as usize,
        );
        let sixel_str = match sixel_image.encode() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::with_capacity(sixel_str.len() + 16);
        let _ = write!(
            out,
            "\x1b[{};{}H",
            scaled.display_row.saturating_add(1),
            scaled.display_col.saturating_add(1)
        );
        out.extend_from_slice(sixel_str.as_bytes());
        out
    }
}

pub fn backend_for(protocol: GraphicsProtocol) -> Option<Box<dyn PreviewBackend>> {
    match protocol {
        GraphicsProtocol::Kitty => Some(Box::new(KittyBackend)),
        GraphicsProtocol::Sixel => Some(Box::new(SixelBackend)),
        GraphicsProtocol::Iterm2 => Some(Box::new(Iterm2Backend)),
        GraphicsProtocol::None => None,
    }
}

// ---------------------------------------------------------------------------
// File classification for the viewer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileViewKind {
    Text,
    Image,
    Unsupported(String),
}

const TEXT_VIEW_LIMIT: u64 = 1_000_000; // 1 MB
const IMAGE_VIEW_LIMIT: u64 = 10_000_000; // 10 MB

/// Decide what kind of viewer to open for `name` (with the given `size`).
pub fn detect_view_kind(name: &str, size: u64) -> FileViewKind {
    if is_previewable_image(name) {
        if size > IMAGE_VIEW_LIMIT {
            return FileViewKind::Unsupported(format!(
                "image too large ({})",
                crate::transfer::format_bytes(size)
            ));
        }
        return FileViewKind::Image;
    }
    if is_viewable_text(name) {
        if size > TEXT_VIEW_LIMIT {
            return FileViewKind::Unsupported(format!(
                "text too large ({})",
                crate::transfer::format_bytes(size)
            ));
        }
        return FileViewKind::Text;
    }
    FileViewKind::Unsupported("unsupported file type".into())
}

/// Heuristic: file extension tells us whether a file is likely an image we
/// can preview.
pub fn is_previewable_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Heuristic: filename suggests a plain-text format. Conservative on purpose —
/// false negatives here just mean the user has to download to see it.
pub fn is_viewable_text(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    // Common bare filenames without extensions.
    if matches!(
        lower.as_str(),
        "readme"
            | "license"
            | "licence"
            | "makefile"
            | "dockerfile"
            | "changelog"
            | "authors"
            | "contributors"
            | "todo"
            | "notice"
    ) {
        return true;
    }

    let ext = match lower.rsplit_once('.') {
        Some((_, e)) => e,
        None => return false,
    };
    matches!(
        ext,
        "txt" | "md" | "rst" | "log" | "ini" | "conf" | "cfg" | "config" | "env"
            | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "htm" | "css"
            | "scss" | "sass" | "less"
            | "js" | "mjs" | "cjs" | "ts" | "jsx" | "tsx"
            | "rs" | "py" | "rb" | "go" | "c" | "h" | "cpp" | "cxx" | "cc" | "hpp"
            | "cs" | "java" | "kt" | "swift" | "php" | "lua" | "pl" | "r"
            | "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat"
            | "sql" | "csv" | "tsv"
            | "gitignore" | "gitattributes" | "editorconfig"
            | "diff" | "patch"
    )
}
