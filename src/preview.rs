//! Image and text preview support.
//!
//! Detects the terminal's graphics protocol (kitty / sixel / iterm2),
//! provides backends that emit the right escape sequences, and classifies
//! files into text / image / unsupported for the viewer.

use std::env;
use std::fmt;
use std::io::{Cursor, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits};

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
    detect_explained(prefer).0
}

/// [`detect`], plus why auto-detection chose nothing when that is worth
/// telling the user: a multiplexer is in the way of a terminal that may
/// well draw images itself.
pub fn detect_explained(prefer: ImagePreviewMode) -> (GraphicsProtocol, Option<&'static str>) {
    detect_explained_from(prefer, |name| env::var(name).ok())
}

/// [`detect`], reading the environment through `var`.
#[cfg(test)]
fn detect_from(prefer: ImagePreviewMode, var: impl Fn(&str) -> Option<String>) -> GraphicsProtocol {
    detect_explained_from(prefer, var).0
}

/// [`detect_explained`], reading the environment through `var`.
fn detect_explained_from(
    prefer: ImagePreviewMode,
    var: impl Fn(&str) -> Option<String>,
) -> (GraphicsProtocol, Option<&'static str>) {
    match prefer {
        ImagePreviewMode::None => return (GraphicsProtocol::None, None),
        ImagePreviewMode::Kitty => return (GraphicsProtocol::Kitty, None),
        ImagePreviewMode::Sixel => return (GraphicsProtocol::Sixel, None),
        ImagePreviewMode::Iterm2 => return (GraphicsProtocol::Iterm2, None),
        ImagePreviewMode::Auto => {}
    }

    let term = var("TERM").unwrap_or_default();
    let term_program = var("TERM_PROGRAM").unwrap_or_default();

    // A multiplexer keeps the outer terminal's variables (KITTY_WINDOW_ID,
    // WT_SESSION, …) but not its graphics: tmux drops kitty's escapes and,
    // without passthrough, iTerm2's, and draws sixel only when built for
    // it. Guessing from those variables left a blank viewer. tmux sets TERM
    // to screen-* by default, so it is asked about first.
    if var("TMUX").is_some() || term.starts_with("tmux") {
        return (
            GraphicsProtocol::None,
            Some(
                "inside tmux, which doesn't pass image escapes on; set image_preview to use one anyway",
            ),
        );
    }
    if var("STY").is_some() || term.starts_with("screen") {
        return (
            GraphicsProtocol::None,
            Some(
                "inside screen, which doesn't pass image escapes on; set image_preview to use one anyway",
            ),
        );
    }

    if term.contains("kitty") || var("KITTY_WINDOW_ID").is_some() {
        return (GraphicsProtocol::Kitty, None);
    }
    if term_program == "ghostty" || var("GHOSTTY_RESOURCES_DIR").is_some() {
        return (GraphicsProtocol::Kitty, None);
    }
    // WezTerm speaks kitty graphics only with `enable_kitty_graphics` set;
    // iTerm2's protocol works out of the box.
    if term_program == "WezTerm" || term_program == "iTerm.app" {
        return (GraphicsProtocol::Iterm2, None);
    }
    // Konsole has supported sixel since ~22.04 and sets KONSOLE_VERSION.
    if var("KONSOLE_VERSION").is_some() {
        return (GraphicsProtocol::Sixel, None);
    }
    // foot says who it is only through TERM (foot, foot-extra).
    if term.starts_with("foot") {
        return (GraphicsProtocol::Sixel, None);
    }
    // mlterm sets MLTERM.
    if var("MLTERM").is_some() {
        return (GraphicsProtocol::Sixel, None);
    }
    // Not xterm: every xterm sets XTERM_VERSION, but only one started with
    // a VT340 terminal id draws sixel, and the rest showed a blank viewer.
    // Set `image_preview = sixel` for one that does.
    //
    // Windows Terminal (≥ 1.22) supports sixel and always sets WT_SESSION
    // (a GUID) regardless of what the shell sets TERM to — including leaving
    // it unset entirely when running under WSL.
    if var("WT_SESSION").is_some() {
        return (GraphicsProtocol::Sixel, None);
    }
    (GraphicsProtocol::None, None)
}

/// What can go wrong rendering an image preview.
///
/// Carried back to the caller so the viewer can surface a useful message
/// instead of a blank pane (`Vec::new()` was the old failure signal).
#[derive(Debug)]
pub enum PreviewError {
    /// The decoded image would need more memory than the viewer allows,
    /// known from its header before any pixel buffer was allocated. Distinct from a decode failure: the file may be
    /// fine, just bigger than the viewer takes.
    TooLarge,
    /// `image::ImageReader::decode` failed — bad bytes, unsupported codec,
    /// truncated download, etc. The message is the decoder's own, not
    /// sanitised here; the log and the viewer sanitise what they show.
    Decode(String),
    /// Re-encoding the scaled RGBA buffer as PNG (kitty / iterm2 path) or
    /// as sixel failed. Should not happen in practice but we surface a real
    /// message rather than collapsing to an empty pane.
    Encode(String),
    /// The terminal panel is zero cells wide or tall. Nothing to render;
    /// callers usually just skip silently.
    EmptyPanel,
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => {
                write!(f, "image too large for preview")
            }
            Self::Decode(msg) => write!(f, "image decode failed: {msg}"),
            Self::Encode(msg) => write!(f, "image encode failed: {msg}"),
            Self::EmptyPanel => write!(f, "viewer panel has no room"),
        }
    }
}

/// What every graphics-protocol implementation must provide.
pub trait PreviewBackend {
    /// Render `image_bytes` (PNG/JPEG/etc.) into the area at terminal cell
    /// `(col, row)` with size `(cols, rows)`. Returns the raw byte sequence
    /// to write to the terminal. `(col, row)` are 0-indexed.
    ///
    /// `Err` propagates a structured reason — the viewer surfaces it as a
    /// log line instead of leaving the user with a blank pane.
    fn render(
        &self,
        image_bytes: &[u8],
        col: u16,
        row: u16,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<u8>, PreviewError>;
}

/// A terminal cell's size in pixels, and whether the terminal said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSize {
    /// `(px_per_col, px_per_row)`.
    pub px: (u32, u32),
    /// False when the terminal reported nothing and [`FALLBACK_CELL`] is in
    /// use.
    pub measured: bool,
}

/// The cell size assumed when the terminal doesn't report one (the Windows
/// console API, some old terminals).
///
/// It errs small. Sixel draws an image at its own pixel size, so a guess
/// larger than the real cells made images spill out of the viewer; one
/// smaller only makes them smaller. Kitty and iTerm2 are told a box of
/// cells when the size is guessed, which they fit the image to.
const FALLBACK_CELL: CellSize = CellSize {
    px: (8, 16),
    measured: false,
};

/// Sixel draws in bands of this many pixel rows, and a band is drawn whole.
const SIXEL_BAND: u32 = 6;

/// Hard cap on bytes the image decoder may allocate, checked against the
/// size the header declares before any pixel buffer exists.
///
/// This is the only bound on an image's dimensions: any width and height
/// that decode within it are shown, scaled down to fit the panel. 512 MiB
/// holds about 170 megapixels at 8-bit RGB (a 48 MP phone photo is
/// 144 MiB) or 130 at RGBA. The buffer lives only while the image is
/// scaled, on a blocking thread, and is freed before anything is drawn.
const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

/// Query the terminal for cell pixel dimensions.
///
/// Uses crossterm's `window_size()` which
/// goes through `TIOCGWINSZ` on Unix. Many terminals — Konsole, kitty, iTerm2,
/// xterm with `-tn xterm-direct`, etc. — populate `ws_xpixel` and `ws_ypixel`
/// correctly. When the report comes back as zero (some old terminals, the
/// Windows console host) we fall back to [`FALLBACK_CELL`].
pub fn cell_pixels() -> CellSize {
    if let Ok(ws) = crossterm::terminal::window_size()
        && ws.columns > 0
        && ws.rows > 0
        && ws.width > 0
        && ws.height > 0
    {
        return CellSize {
            px: (
                (ws.width as u32 / ws.columns as u32).max(1),
                (ws.height as u32 / ws.rows as u32).max(1),
            ),
            measured: true,
        };
    }
    FALLBACK_CELL
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
    /// Whether the cell size was measured. When it was, `width_px` by
    /// `height_px` is the size to show the image at; when guessed, the
    /// image is fitted to `cells_w` by `cells_h` cells instead.
    measured: bool,
}

/// Decode the image, downscale to fit the panel area while preserving the
/// aspect ratio, and compute the centered display offset.
///
/// Never upscales: if the image is smaller than the panel, it stays at its
/// native pixel size and is centered.
///
/// `cell` is the terminal's cell size; see [`cell_pixels`].
///
/// The image is turned the way its EXIF orientation says: phone photos are
/// stored unrotated, and shown as stored they came out sideways.
fn scale_for_cells(
    image_bytes: &[u8],
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    cell: CellSize,
) -> Result<ScaledForCells, PreviewError> {
    let (px_per_col, px_per_row) = cell.px;
    if cols == 0 || rows == 0 {
        return Err(PreviewError::EmptyPanel);
    }

    // Use ImageReader with an explicit allocation limit so a hostile image
    // cannot OOM us: a highly-compressed PNG declaring 20k × 20k would
    // otherwise allocate 1.6 GB inside the decoder. `total_bytes` below
    // refuses it from the header; `max_alloc` covers what decoders allocate
    // on the way.
    let mut reader = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    let mut limits = Limits::default();
    // No cap on width or height (the default is 65535 a side): memory is
    // what matters, and any shape that fits in it is scaled down.
    limits.max_image_width = None;
    limits.max_image_height = None;
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let decode_error = |e: ImageError| match e {
        ImageError::Limits(_) => PreviewError::TooLarge,
        e => PreviewError::Decode(e.to_string()),
    };
    // Through the decoder rather than `decode()`, which would not say how
    // the image is meant to be turned. `decode()` also checked the whole
    // buffer against `max_alloc`; that check is made here instead.
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    if decoder.total_bytes() > MAX_IMAGE_ALLOC {
        return Err(PreviewError::TooLarge);
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    // A quarter turn swaps width and height: the fit is worked out for the
    // shape the image is seen in.
    let quarter_turn = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let img = DynamicImage::from_decoder(decoder).map_err(decode_error)?;

    let (sw, sh) = (img.width().max(1), img.height().max(1));
    let (iw, ih) = if quarter_turn { (sh, sw) } else { (sw, sh) };

    let panel_w_px = u32::from(cols).saturating_mul(px_per_col).max(1);
    // Whole sixel bands only: the last band is drawn whole, so an image as
    // tall as the panel ran up to five pixels past it. The other protocols
    // lose at most five pixels of height.
    let panel_h_px = u32::from(rows).saturating_mul(px_per_row);
    let panel_h_px = (panel_h_px - panel_h_px % SIXEL_BAND).max(1);

    // Aspect-preserving scale factor (downscale only).
    let scale_w = panel_w_px as f64 / iw as f64;
    let scale_h = panel_h_px as f64 / ih as f64;
    let scale = scale_w.min(scale_h).min(1.0);

    let scaled_w = ((iw as f64 * scale) as u32).max(1);
    let scaled_h = ((ih as f64 * scale) as u32).max(1);

    // Scale first, in the stored shape, and turn the small result: turning
    // first copied the whole decoded image, doubling the peak memory of a
    // large photo for nothing.
    //
    // `thumbnail_exact`, not a Lanczos3 `resize_exact`: that one keeps a
    // 32-bit float copy of the whole source width at the new height, which
    // for a large image is far bigger than the image itself. The box filter
    // works pixel by pixel and only ever shrinks here.
    let (tw, th) = if quarter_turn {
        (scaled_h, scaled_w)
    } else {
        (scaled_w, scaled_h)
    };
    let mut small = if tw == sw && th == sh {
        img
    } else {
        let small = img.thumbnail_exact(tw, th);
        drop(img);
        small
    };
    small.apply_orientation(orientation);
    let resized = small.into_rgba8();
    // What was actually made, which everything below describes: should the
    // two disagree, the escape would misdescribe its own pixels.
    let (scaled_w, scaled_h) = resized.dimensions();

    // Cell extent of the scaled image, rounded up so we don't truncate.
    let used_w_cells = scaled_w.div_ceil(px_per_col).max(1).min(u32::from(cols)) as u16;
    let used_h_cells = scaled_h.div_ceil(px_per_row).max(1).min(u32::from(rows)) as u16;

    let offset_x = cols.saturating_sub(used_w_cells) / 2;
    let offset_y = rows.saturating_sub(used_h_cells) / 2;

    Ok(ScaledForCells {
        rgba: resized.into_raw(),
        width_px: scaled_w,
        height_px: scaled_h,
        display_col: col.saturating_add(offset_x),
        display_row: row.saturating_add(offset_y),
        cells_w: used_w_cells,
        cells_h: used_h_cells,
        measured: cell.measured,
    })
}

/// Encode an RGBA buffer as PNG. Used by the kitty and iTerm2 backends after
/// scaling.
fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, image::ImageError> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or_else(|| {
        image::ImageError::Parameter(image::error::ParameterError::from_kind(
            image::error::ParameterErrorKind::DimensionMismatch,
        ))
    })?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

/// Kitty graphics protocol — chunked base64 PNG transmission.
///
/// Supported by kitty, ghostty, and WezTerm with `enable_kitty_graphics`.
pub struct KittyBackend;

impl PreviewBackend for KittyBackend {
    fn render(
        &self,
        image_bytes: &[u8],
        col: u16,
        row: u16,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<u8>, PreviewError> {
        kitty_escape(&scale_for_cells(
            image_bytes,
            col,
            row,
            cols,
            rows,
            cell_pixels(),
        )?)
    }
}

/// The kitty escape sequence that draws `scaled`.
fn kitty_escape(scaled: &ScaledForCells) -> Result<Vec<u8>, PreviewError> {
    let png = encode_png_rgba(&scaled.rgba, scaled.width_px, scaled.height_px)
        .map_err(|e| PreviewError::Encode(e.to_string()))?;

    let b64 = BASE64.encode(&png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    if chunks.is_empty() {
        return Err(PreviewError::Encode("empty PNG payload".into()));
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
            // q=2 suppresses kitty's per-image response strings. With the
            // cell size measured, the PNG is already the size to show, and
            // a cell box (rounded up to whole cells) only stretched it:
            // a 12x8 icon filled 2x1 cells as a 20x20 square. Guessed, the
            // box is what keeps it inside the panel.
            let size = if scaled.measured {
                String::new()
            } else {
                format!("c={},r={},", scaled.cells_w, scaled.cells_h)
            };
            let _ = write!(out, "\x1b_Gf=100,a=T,{size}q=2,m={m};");
        } else {
            let _ = write!(out, "\x1b_Gm={m};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    Ok(out)
}

/// iTerm2 inline image protocol — single OSC 1337 escape with base64 payload.
///
/// Supported by iTerm2 and WezTerm.
pub struct Iterm2Backend;

impl PreviewBackend for Iterm2Backend {
    fn render(
        &self,
        image_bytes: &[u8],
        col: u16,
        row: u16,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<u8>, PreviewError> {
        iterm2_escape(&scale_for_cells(
            image_bytes,
            col,
            row,
            cols,
            rows,
            cell_pixels(),
        )?)
    }
}

/// The iTerm2 escape sequence that draws `scaled`.
fn iterm2_escape(scaled: &ScaledForCells) -> Result<Vec<u8>, PreviewError> {
    let png = encode_png_rgba(&scaled.rgba, scaled.width_px, scaled.height_px)
        .map_err(|e| PreviewError::Encode(e.to_string()))?;
    let b64 = BASE64.encode(&png);
    let mut out = Vec::with_capacity(b64.len() + 128);
    let _ = write!(
        out,
        "\x1b[{};{}H",
        scaled.display_row.saturating_add(1),
        scaled.display_col.saturating_add(1)
    );
    // In pixels when the cell size is measured: in cells, iTerm2 scales
    // the image up to fill them. Guessed, the cells keep it in the panel.
    let (width, height) = if scaled.measured {
        (
            format!("{}px", scaled.width_px),
            format!("{}px", scaled.height_px),
        )
    } else {
        (scaled.cells_w.to_string(), scaled.cells_h.to_string())
    };
    let _ = write!(
        out,
        "\x1b]1337;File=inline=1;width={width};height={height};preserveAspectRatio=1:{b64}\x07",
    );
    Ok(out)
}

/// Sixel — DEC's sixel graphics protocol. Encoded via `icy_sixel`'s pure-Rust
/// implementation. Supported by Konsole (≥22.04), mlterm, foot, Windows
/// Terminal (≥1.22), xterm started with `-ti vt340`, and others.
pub struct SixelBackend;

impl PreviewBackend for SixelBackend {
    fn render(
        &self,
        image_bytes: &[u8],
        col: u16,
        row: u16,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<u8>, PreviewError> {
        sixel_escape(scale_for_cells(
            image_bytes,
            col,
            row,
            cols,
            rows,
            cell_pixels(),
        )?)
    }
}

/// The sixel escape sequence that draws `scaled`.
fn sixel_escape(scaled: ScaledForCells) -> Result<Vec<u8>, PreviewError> {
    let sixel_image = icy_sixel::SixelImage::from_rgba(
        scaled.rgba,
        scaled.width_px as usize,
        scaled.height_px as usize,
    );
    let sixel_str = sixel_image
        .encode()
        .map_err(|e| PreviewError::Encode(format!("{e:?}")))?;

    let mut out = Vec::with_capacity(sixel_str.len() + 16);
    let _ = write!(
        out,
        "\x1b[{};{}H",
        scaled.display_row.saturating_add(1),
        scaled.display_col.saturating_add(1)
    );
    out.extend_from_slice(sixel_str.as_bytes());
    Ok(out)
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

pub(crate) const TEXT_VIEW_LIMIT: u64 = 1_000_000; // 1 MB

/// Largest image accepted for preview, checked against the size the server
/// reports in its directory listing.
///
/// The transports' `read_to_bytes` caps derive from this rather than
/// restating it: that cap is the backstop for a server that under-reports a
/// file's size in its listing, so if it were ever the *smaller* of the two,
/// every image between the two values would silently truncate instead of
/// being refused. Keeping one definition makes them impossible to drift.
///
/// Note this bounds bytes fetched, not decode memory — [`MAX_IMAGE_ALLOC`]
/// bounds that independently, and an oversized declaration is refused from
/// the header before a pixel buffer is allocated.
pub(crate) const IMAGE_VIEW_LIMIT: u64 = 25_000_000; // 25 MB

/// Compile-time guard on the relationship described above.
///
/// Both transports currently *derive* their cap from this constant, so this
/// holds trivially — the point is that it keeps holding if someone later
/// writes a literal back into either one. A transport cap below the viewer's
/// limit wouldn't fail loudly: images between the two values would come back
/// truncated and fail to decode with a confusing error, instead of being
/// cleanly refused before the fetch. Failing the build beats a test here,
/// since there is no runtime state to observe.
const _: () = {
    assert!(crate::transport::sftp::MAX_PREVIEW_BYTES >= IMAGE_VIEW_LIMIT);
    assert!(crate::transport::ftp_impl::MAX_PREVIEW_BYTES >= IMAGE_VIEW_LIMIT);
};

/// Decide what kind of viewer to open for `name` (with the given `size`).
pub fn detect_view_kind(name: &str, size: u64) -> FileViewKind {
    let kind = if is_previewable_image(name) {
        FileViewKind::Image
    } else if is_viewable_text(name) {
        FileViewKind::Text
    } else {
        return FileViewKind::Unsupported("unsupported file type".into());
    };
    too_large(&kind, size)
        .map(FileViewKind::Unsupported)
        .unwrap_or(kind)
}

/// The most bytes a viewer of `kind` accepts.
pub(crate) fn view_limit(kind: &FileViewKind) -> u64 {
    match kind {
        FileViewKind::Text => TEXT_VIEW_LIMIT,
        FileViewKind::Image => IMAGE_VIEW_LIMIT,
        FileViewKind::Unsupported(_) => 0,
    }
}

/// Why `size` bytes are too many for a viewer of `kind`, if they are.
///
/// Checked twice: against the size the listing reports, before fetching,
/// and against the bytes that actually arrive. The listing can understate a
/// file, or the file can grow in between, and a remote fetch is capped only
/// at the image limit — for text, 25 times the limit, all of it sanitized
/// and tokenized on the UI thread.
pub(crate) fn too_large(kind: &FileViewKind, size: u64) -> Option<String> {
    let what = match kind {
        FileViewKind::Text => "text",
        FileViewKind::Image => "image",
        FileViewKind::Unsupported(_) => return None,
    };
    (size > view_limit(kind))
        .then(|| format!("{what} too large ({})", crate::transfer::format_bytes(size)))
}

/// Heuristic: file extension tells us whether a file is likely an image we
/// can preview.
pub fn is_previewable_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Heuristic: filename suggests a plain-text format. Conservative on purpose —
/// false negatives here just mean the user has to download to see it.
pub fn is_viewable_text(name: &str) -> bool {
    // Anything the highlighter has a language for is text. Kept apart, the
    // two lists drifted: `.bashrc`, `justfile` and `.pyi` were highlighted
    // but never opened.
    if crate::highlight::lang_for_name(name) != crate::highlight::Lang::Unknown {
        return true;
    }
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
        "txt"
            | "md"
            | "rst"
            | "log"
            | "ini"
            | "conf"
            | "cfg"
            | "config"
            | "env"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "py"
            | "rb"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "cxx"
            | "cc"
            | "hpp"
            | "cs"
            | "java"
            | "kt"
            | "swift"
            | "php"
            | "lua"
            | "pl"
            | "r"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "bat"
            | "sql"
            | "csv"
            | "tsv"
            | "gitignore"
            | "gitattributes"
            | "editorconfig"
            | "diff"
            | "patch"
            | "nfo"
    )
}

/// Returns `true` if the file is an NFO file (CP437-encoded release info).
pub fn is_nfo_file(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".nfo")
}

/// Decode an `.nfo` file.
///
/// Scene release notes are CP437, but media managers such as Kodi write
/// `.nfo` files as UTF-8 XML, whose accented letters CP437 mangled (`é`
/// became `├⌐`). Valid UTF-8 is read as UTF-8, less any byte-order mark:
/// CP437 art, built from bytes 0xB0–0xDF, almost never is. Plain ASCII
/// reads the same either way.
pub fn decode_nfo(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => decode_cp437(bytes),
    }
}

/// Decode CP437-encoded bytes to a UTF-8 `String`.
///
/// NFO files use DOS codepage 437; bytes 0x80–0xFF map to box-drawing
/// characters and other symbols that would otherwise appear as replacement
/// characters under `from_utf8_lossy`.
pub fn decode_cp437(bytes: &[u8]) -> String {
    #[rustfmt::skip]
    const MAP: [char; 128] = [
        // 0x80–0x8F
        'Ç','ü','é','â','ä','à','å','ç','ê','ë','è','ï','î','ì','Ä','Å',
        // 0x90–0x9F
        'É','æ','Æ','ô','ö','ò','û','ù','ÿ','Ö','Ü','¢','£','¥','₧','ƒ',
        // 0xA0–0xAF
        'á','í','ó','ú','ñ','Ñ','ª','º','¿','⌐','¬','½','¼','¡','«','»',
        // 0xB0–0xBF
        '░','▒','▓','│','┤','╡','╢','╖','╕','╣','║','╗','╝','╜','╛','┐',
        // 0xC0–0xCF
        '└','┴','┬','├','─','┼','╞','╟','╚','╔','╩','╦','╠','═','╬','╧',
        // 0xD0–0xDF
        '╨','╤','╥','╙','╘','╒','╓','╫','╪','┘','┌','█','▄','▌','▐','▀',
        // 0xE0–0xEF
        'α','ß','Γ','π','Σ','σ','µ','τ','Φ','Θ','Ω','δ','∞','φ','ε','∩',
        // 0xF0–0xFF
        '≡','±','≥','≤','⌠','⌡','÷','≈','°','∙','·','√','ⁿ','²','■','\u{00A0}',
    ];
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                MAP[(b - 0x80) as usize]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_at_the_limit_is_viewable() {
        assert_eq!(
            detect_view_kind("photo.png", IMAGE_VIEW_LIMIT),
            FileViewKind::Image,
            "a file exactly at the cap must still open",
        );
    }

    #[test]
    fn image_over_the_limit_is_refused_with_its_size() {
        let kind = detect_view_kind("photo.png", IMAGE_VIEW_LIMIT + 1);
        match kind {
            FileViewKind::Unsupported(reason) => {
                assert!(reason.contains("too large"), "{reason}");
                // The message quotes the actual size so the user can judge
                // whether the cap or the file is the surprising part.
                assert!(reason.contains("MiB"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn text_limit_is_unchanged_by_the_image_cap() {
        // The two are independent; raising images must not quietly widen the
        // text viewer, whose load path tokenises synchronously on the UI loop.
        assert_eq!(
            detect_view_kind("notes.txt", TEXT_VIEW_LIMIT),
            FileViewKind::Text,
        );
        assert!(matches!(
            detect_view_kind("notes.txt", TEXT_VIEW_LIMIT + 1),
            FileViewKind::Unsupported(_),
        ));
    }

    #[test]
    fn unknown_extension_is_unsupported_regardless_of_size() {
        assert!(matches!(
            detect_view_kind("archive.tar.zst", 10),
            FileViewKind::Unsupported(_),
        ));
    }

    /// The sixel backend is the one preview path whose output only a terminal
    /// could judge: kitty and iTerm2 carry PNG, which the suite can compare,
    /// but sixel is a palette-quantised escape stream. Decoding it back and
    /// bounding the error against what was encoded makes a broken or degraded
    /// encoder a test failure rather than a manual check after every
    /// `icy_sixel` bump.
    ///
    /// Compared against the *scaled* RGBA, not the source PNG, so this tests
    /// encoding and not the resize. The source mixes smooth gradients (which
    /// quantisation has to approximate) with hard edges (which it must not
    /// smear).
    #[test]
    fn sixel_output_decodes_back_to_the_image_it_was_given() {
        let (w, h) = (240u32, 160u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let edge = ((x / 20) + (y / 20)) % 2 == 0;
                rgba.extend_from_slice(&[
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    if edge { 220 } else { 30 },
                    255,
                ]);
            }
        }
        let png = encode_png_rgba(&rgba, w, h).unwrap();
        let scaled = scale_for_cells(&png, 0, 0, 400, 200, CELL).unwrap();

        let sixel = icy_sixel::SixelImage::from_rgba(
            scaled.rgba.clone(),
            scaled.width_px as usize,
            scaled.height_px as usize,
        )
        .encode()
        .unwrap();
        let decoded = icy_sixel::SixelImage::decode(sixel.as_bytes()).unwrap();

        let (sw, sh) = (scaled.width_px as usize, scaled.height_px as usize);
        assert_eq!(decoded.width, sw, "decoded width");
        assert!(
            decoded.height >= sh && decoded.height < sh + 6,
            "decoded height {} for a {sh}-row image; sixel pads to a six-row band at most",
            decoded.height,
        );

        let mut total = 0u64;
        for y in 0..sh {
            for x in 0..sw {
                let got = &decoded.pixels[(y * decoded.width + x) * 4..][..3];
                let want = &scaled.rgba[(y * sw + x) * 4..][..3];
                for c in 0..3 {
                    total += u64::from(got[c].abs_diff(want[c]));
                }
            }
        }
        let mean = total as f64 / (sw * sh * 3) as f64;
        assert!(
            mean < 10.0,
            "mean absolute error {mean:.2}/255 after a sixel round trip",
        );
    }

    // --- protocol detection ---

    fn detect_with(prefer: ImagePreviewMode, vars: &[(&str, &str)]) -> GraphicsProtocol {
        detect_from(prefer, |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn a_configured_protocol_wins_over_the_environment() {
        let kitty = [("TERM", "xterm-kitty")];
        assert_eq!(
            detect_with(ImagePreviewMode::None, &kitty),
            GraphicsProtocol::None
        );
        assert_eq!(
            detect_with(ImagePreviewMode::Sixel, &kitty),
            GraphicsProtocol::Sixel
        );
        assert_eq!(
            detect_with(ImagePreviewMode::Iterm2, &kitty),
            GraphicsProtocol::Iterm2
        );
        assert_eq!(
            detect_with(ImagePreviewMode::Kitty, &[]),
            GraphicsProtocol::Kitty
        );
    }

    #[test]
    fn auto_detects_each_terminal_from_its_environment() {
        use GraphicsProtocol::*;
        let cases: &[(&[(&str, &str)], GraphicsProtocol)] = &[
            (&[("TERM", "xterm-kitty")], Kitty),
            (&[("KITTY_WINDOW_ID", "1")], Kitty),
            // WezTerm speaks kitty graphics only when configured to; iTerm2
            // images work out of the box.
            (&[("TERM_PROGRAM", "WezTerm")], Iterm2),
            (&[("TERM", "foot")], Sixel),
            (&[("TERM", "foot-extra")], Sixel),
            (&[("TERM_PROGRAM", "ghostty")], Kitty),
            (&[("GHOSTTY_RESOURCES_DIR", "/g")], Kitty),
            (&[("TERM_PROGRAM", "iTerm.app")], Iterm2),
            (&[("KONSOLE_VERSION", "230800")], Sixel),
            (&[("MLTERM", "3.9")], Sixel),
            // Every xterm sets XTERM_VERSION; only one configured for it
            // draws sixel, so xterm is left to the config.
            (
                &[("TERM", "xterm-256color"), ("XTERM_VERSION", "XTerm(390)")],
                None,
            ),
            (&[("WT_SESSION", "guid")], Sixel),
            // xterm without a version could be anything claiming to be xterm.
            (&[("TERM", "xterm-256color")], None),
            (&[], None),
        ];
        for (vars, want) in cases {
            assert_eq!(detect_with(ImagePreviewMode::Auto, vars), *want, "{vars:?}");
        }
    }

    /// tmux and screen keep the outer terminal's variables but don't pass
    /// its graphics escapes on, so auto picks nothing, and says why.
    #[test]
    fn auto_picks_nothing_inside_a_multiplexer() {
        let cases: &[&[(&str, &str)]] = &[
            &[
                ("TMUX", "/tmp/tmux-1000/default,1,0"),
                ("KITTY_WINDOW_ID", "1"),
            ],
            &[("TERM", "tmux-256color"), ("WT_SESSION", "guid")],
            &[("TERM", "screen-256color"), ("KONSOLE_VERSION", "230800")],
            &[("STY", "1234.pts-0.host"), ("TERM_PROGRAM", "iTerm.app")],
        ];
        for vars in cases {
            let lookup = |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            };
            let (proto, why) = detect_explained_from(ImagePreviewMode::Auto, lookup);
            assert_eq!(proto, GraphicsProtocol::None, "{vars:?}");
            let why = why.unwrap_or_default();
            assert!(
                why.contains("tmux") || why.contains("screen"),
                "{vars:?}: {why}"
            );
        }
    }

    /// Configured explicitly, a protocol is used inside tmux too: tmux 3.4
    /// can draw sixel, and passthrough can carry the others.
    #[test]
    fn a_configured_protocol_is_used_inside_a_multiplexer() {
        assert_eq!(
            detect_with(ImagePreviewMode::Sixel, &[("TMUX", "x")]),
            GraphicsProtocol::Sixel
        );
    }

    #[test]
    fn kitty_outranks_a_sixel_terminal_it_runs_inside() {
        let vars = [("KITTY_WINDOW_ID", "1"), ("WT_SESSION", "guid")];
        assert_eq!(
            detect_with(ImagePreviewMode::Auto, &vars),
            GraphicsProtocol::Kitty
        );
    }

    #[test]
    fn every_protocol_but_none_has_a_backend() {
        for p in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Sixel,
            GraphicsProtocol::Iterm2,
        ] {
            assert!(backend_for(p).is_some(), "{p:?}");
        }
        assert!(backend_for(GraphicsProtocol::None).is_none());
    }

    // --- scaling ---

    /// A `w`x`h` PNG of noise, which compresses badly: big enough payloads
    /// to need more than one kitty chunk.
    fn noise_png(w: u32, h: u32) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let rgba: Vec<u8> = (0..w * h * 4)
            .map(|i| {
                if i % 4 == 3 {
                    return 255;
                }
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        encode_png_rgba(&rgba, w, h).unwrap()
    }

    /// A cell size the terminal reported.
    const CELL: CellSize = CellSize {
        px: (10, 20),
        measured: true,
    };
    /// The same size, guessed because the terminal reported none.
    const GUESSED: CellSize = CellSize {
        px: (10, 20),
        measured: false,
    };

    #[test]
    fn a_small_image_keeps_its_size_and_is_centred() {
        // 100x40 px is 10x2 cells; the panel is 20x10 cells at (5, 3).
        let s = scale_for_cells(&noise_png(100, 40), 5, 3, 20, 10, CELL).unwrap();
        assert_eq!((s.width_px, s.height_px), (100, 40));
        assert_eq!((s.cells_w, s.cells_h), (10, 2));
        assert_eq!((s.display_col, s.display_row), (10, 7));
        assert_eq!(s.rgba.len(), 100 * 40 * 4);
    }

    #[test]
    fn a_large_image_is_scaled_down_keeping_its_shape() {
        // The panel is 200x200 px; 400x100 halves to 200x50.
        let s = scale_for_cells(&noise_png(400, 100), 0, 0, 20, 10, CELL).unwrap();
        assert_eq!((s.width_px, s.height_px), (200, 50));
        assert_eq!((s.cells_w, s.cells_h), (20, 3));
        assert_eq!((s.display_col, s.display_row), (0, 3));
    }

    #[test]
    fn an_empty_panel_is_refused_before_decoding() {
        let err = scale_for_cells(b"not even an image", 0, 0, 0, 10, CELL);
        assert!(matches!(err, Err(PreviewError::EmptyPanel)));
    }

    #[test]
    fn bytes_that_are_not_an_image_fail_to_decode() {
        let err = scale_for_cells(b"not an image", 0, 0, 10, 10, CELL);
        assert!(
            matches!(err, Err(PreviewError::Decode(_))),
            "{:?}",
            err.err()
        );
    }

    /// A PNG declaring `w` x `h` RGBA, with no pixel data at all.
    fn png_header_only(w: u32, h: u32) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // The decoder reads up to the first data chunk before it reports the
        // size, so give it one: a zlib header and nothing after it.
        let idat = b"IDAT\x78\x9c";
        png.extend_from_slice(&2u32.to_be_bytes());
        png.extend_from_slice(idat);
        png.extend_from_slice(&crc32(idat).to_be_bytes());
        png
    }

    /// Too big to hold decoded (20000 x 20000 RGBA is 1.6 GB): refused from
    /// the header, before a pixel buffer is allocated.
    #[test]
    fn an_image_too_big_to_decode_is_refused_unread() {
        let err = scale_for_cells(&png_header_only(20_000, 20_000), 0, 0, 10, 10, CELL);
        assert!(
            matches!(err, Err(PreviewError::TooLarge)),
            "{:?}",
            err.err()
        );
    }

    /// Past 4096 px a side the image was refused outright, however little
    /// memory it needed. Now only memory limits it, and it's scaled to fit.
    #[test]
    fn an_image_wider_than_4096_px_is_scaled_to_fit() {
        // The panel is 200x200 px; 6000x600 shrinks to 200x20.
        let s = scale_for_cells(&noise_png(6000, 600), 0, 0, 20, 10, CELL).unwrap();
        assert_eq!((s.width_px, s.height_px), (200, 20));
        assert_eq!(s.rgba.len(), 200 * 20 * 4);
    }

    #[test]
    fn a_header_past_the_old_4096_px_cap_is_not_refused_as_too_large() {
        // 5000x5000 RGBA is 100 MB decoded: within the memory cap. With no
        // pixel data it fails to decode, but not for being too large.
        let err = scale_for_cells(&png_header_only(5000, 5000), 0, 0, 10, 10, CELL);
        assert!(
            matches!(err, Err(PreviewError::Decode(_))),
            "{:?}",
            err.err()
        );
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn preview_errors_read_as_sentences() {
        assert_eq!(
            PreviewError::TooLarge.to_string(),
            "image too large for preview"
        );
        assert_eq!(
            PreviewError::Decode("bad".into()).to_string(),
            "image decode failed: bad"
        );
        assert_eq!(
            PreviewError::Encode("bad".into()).to_string(),
            "image encode failed: bad"
        );
        assert_eq!(
            PreviewError::EmptyPanel.to_string(),
            "viewer panel has no room"
        );
    }

    // --- escape sequences ---

    fn png_dimensions(png: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(png, ImageFormat::Png).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn kitty_sends_the_png_in_chunks_after_moving_the_cursor() {
        let scaled = scale_for_cells(&noise_png(100, 40), 5, 3, 20, 10, CELL).unwrap();
        let out = String::from_utf8(kitty_escape(&scaled).unwrap()).unwrap();

        // 1-based row;col of the centred top-left cell.
        let rest = out.strip_prefix("\x1b[8;11H").expect(&out[..20]);
        let chunks: Vec<&str> = rest.split_terminator("\x1b\\").collect();
        assert!(chunks.len() > 1, "a 16 KB payload takes several chunks");

        let mut b64 = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let last = i == chunks.len() - 1;
            let header = if i == 0 {
                // A measured cell size: the PNG is already the size to
                // show, so kitty is not told a cell box to stretch it to.
                format!("\x1b_Gf=100,a=T,q=2,m={};", u8::from(!last))
            } else {
                format!("\x1b_Gm={};", u8::from(!last))
            };
            let data = chunk
                .strip_prefix(&header)
                .unwrap_or_else(|| panic!("chunk {i}"));
            assert!(data.len() <= 4096, "chunk {i} is {} bytes", data.len());
            b64.push_str(data);
        }
        let png = BASE64.decode(b64).unwrap();
        assert_eq!(png_dimensions(&png), (100, 40));
    }

    #[test]
    fn iterm2_sends_one_inline_file_escape() {
        let scaled = scale_for_cells(&noise_png(20, 20), 0, 0, 20, 10, CELL).unwrap();
        let out = String::from_utf8(iterm2_escape(&scaled).unwrap()).unwrap();

        let rest = out.strip_prefix("\x1b[5;10H").expect(&out[..12]);
        let b64 = rest
            .strip_prefix("\x1b]1337;File=inline=1;width=20px;height=20px;preserveAspectRatio=1:")
            .and_then(|r| r.strip_suffix('\x07'))
            .expect("one OSC 1337 sequence");
        let png = BASE64.decode(b64).unwrap();
        assert_eq!(png_dimensions(&png), (20, 20));
    }

    #[test]
    fn sixel_moves_the_cursor_then_sends_the_sixel_stream() {
        let scaled = scale_for_cells(&noise_png(20, 20), 0, 0, 20, 10, CELL).unwrap();
        let out = sixel_escape(scaled).unwrap();
        let sixel = out.strip_prefix(b"\x1b[5;10H").expect("cursor move first");
        let decoded = icy_sixel::SixelImage::decode(sixel).unwrap();
        assert_eq!(decoded.width, 20);
    }

    /// With the cell size guessed, the image is fitted to a box of cells
    /// instead, which keeps it inside the panel whatever the real size.
    #[test]
    fn a_guessed_cell_size_sizes_the_image_in_cells() {
        let scaled = scale_for_cells(&noise_png(100, 40), 5, 3, 20, 10, GUESSED).unwrap();
        let kitty = String::from_utf8(kitty_escape(&scaled).unwrap()).unwrap();
        assert!(
            kitty.contains("\x1b_Gf=100,a=T,c=10,r=2,q=2,"),
            "{}",
            &kitty[..40]
        );
        let iterm = String::from_utf8(iterm2_escape(&scaled).unwrap()).unwrap();
        assert!(iterm.contains(";width=10;height=2;"), "{}", &iterm[..60]);
    }

    /// A tiny image is no longer blown up to fill whole cells: kitty used
    /// to be told 2x1 cells for a 12x8 icon, and stretched it to 20x20.
    #[test]
    fn a_tiny_image_is_not_stretched_to_whole_cells() {
        let scaled = scale_for_cells(&noise_png(12, 8), 0, 0, 20, 10, CELL).unwrap();
        let kitty = String::from_utf8(kitty_escape(&scaled).unwrap()).unwrap();
        assert!(!kitty.contains("c="), "{}", &kitty[..40]);
        assert!(!kitty.contains("r="), "{}", &kitty[..40]);
    }

    /// Sixel draws in bands six pixels tall, and the last band is drawn
    /// whole. An image as tall as the panel ran up to five pixels past it.
    #[test]
    fn a_tall_image_fits_whole_sixel_bands_inside_the_panel() {
        // The panel is 10 rows of 20 px: 200 px, of which 198 are whole bands.
        let s = scale_for_cells(&noise_png(100, 1000), 0, 0, 20, 10, CELL).unwrap();
        assert!(s.height_px <= 198, "{}", s.height_px);
        assert_eq!(s.height_px % 6, 0, "{}", s.height_px);
    }

    // When the terminal reports no cell size, the guess errs small, so an
    // image drawn at its own pixel size (sixel) stays inside the panel on a
    // terminal with small cells. Checked when the tests build.
    const _: () =
        assert!(!FALLBACK_CELL.measured && FALLBACK_CELL.px.0 <= 8 && FALLBACK_CELL.px.1 <= 16);

    /// A JPEG holding EXIF orientation 6 ("rotate 90° clockwise to view"):
    /// stored 40 wide and 20 tall, it is seen 20 wide and 40 tall.
    fn rotated_jpeg() -> Vec<u8> {
        use image::ImageEncoder;
        // Big-endian TIFF: one IFD entry, Orientation (0x0112), SHORT, 6.
        let exif: Vec<u8> = [
            b"MM\0\x2a\0\0\0\x08".as_slice(),
            &[0, 1],
            &[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0],
            &[0, 0, 0, 0],
        ]
        .concat();
        let img = image::RgbImage::from_pixel(40, 20, image::Rgb([200, 30, 30]));
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut out);
        enc.set_exif_metadata(exif).unwrap();
        enc.write_image(img.as_raw(), 40, 20, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// Phone photos store their pixels unrotated and say how to turn them
    /// in EXIF; shown as stored, they came out sideways.
    #[test]
    fn a_photo_is_turned_the_way_its_exif_says() {
        let s = scale_for_cells(&rotated_jpeg(), 0, 0, 80, 40, CELL).unwrap();
        assert_eq!((s.width_px, s.height_px), (20, 40));
    }

    /// Scaled before it's turned, to spare turning the full-size image: the
    /// fit has to be worked out for the shape it's seen in, not stored in.
    #[test]
    fn a_turned_photo_is_fitted_in_the_shape_it_is_seen() {
        // Seen 20x40; the panel is 10x18 px (20 rounded down to whole sixel
        // bands), so it fits at 9x18.
        let s = scale_for_cells(&rotated_jpeg(), 0, 0, 1, 1, CELL).unwrap();
        assert_eq!((s.width_px, s.height_px), (9, 18));
        assert_eq!(s.rgba.len(), 9 * 18 * 4);
    }

    /// Each backend end to end, at whatever cell size this terminal reports:
    /// an image small enough to fit any panel comes out as an escape
    /// sequence that starts by placing the cursor.
    #[test]
    fn every_backend_renders_an_image() {
        let png = noise_png(8, 8);
        for p in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Sixel,
            GraphicsProtocol::Iterm2,
        ] {
            let out = backend_for(p).unwrap().render(&png, 0, 0, 40, 20).unwrap();
            assert!(out.starts_with(b"\x1b["), "{p:?}");
        }
    }

    #[test]
    fn a_backend_reports_an_empty_panel() {
        let png = noise_png(8, 8);
        for p in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Sixel,
            GraphicsProtocol::Iterm2,
        ] {
            let err = backend_for(p).unwrap().render(&png, 0, 0, 40, 0);
            assert!(matches!(err, Err(PreviewError::EmptyPanel)), "{p:?}");
        }
    }

    // --- NFO text ---

    /// Media managers such as Kodi write `.nfo` files as UTF-8 XML; read as
    /// CP437, `é` came out as `├⌐`.
    #[test]
    fn a_utf8_nfo_is_read_as_utf8() {
        let xml = "<title>Amélie</title>";
        assert_eq!(decode_nfo(xml.as_bytes()), xml);
        let with_bom = [b"\xEF\xBB\xBF".as_slice(), xml.as_bytes()].concat();
        assert_eq!(decode_nfo(&with_bom), xml, "the byte-order mark is dropped");
    }

    /// Scene NFO art is CP437, which is almost never valid UTF-8.
    #[test]
    fn a_cp437_nfo_is_read_as_cp437() {
        assert_eq!(decode_nfo(b"\xDB\xDB\xB0 hi"), "██░ hi");
    }

    #[test]
    fn cp437_keeps_ascii_and_maps_the_high_half() {
        assert_eq!(decode_cp437(b"plain ASCII\n"), "plain ASCII\n");
        assert_eq!(
            decode_cp437(&[0xC9, 0xCD, 0xBB, 0xB0, 0xDB, 0x80, 0xE1, 0xFF]),
            "╔═╗░█Çß\u{00A0}"
        );
    }

    #[test]
    fn every_cp437_byte_decodes_to_exactly_one_char() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode_cp437(&all).chars().count(), 256);
    }

    #[test]
    fn nfo_files_are_recognised_by_extension_in_any_case() {
        assert!(is_nfo_file("release.nfo"));
        assert!(is_nfo_file("RELEASE.NFO"));
        assert!(!is_nfo_file("nfo.txt"));
    }

    /// Every file the highlighter has a language for opens in the viewer:
    /// `.bashrc`, `justfile` and `.pyi` were highlighted but never shown.
    #[test]
    fn whatever_the_highlighter_knows_is_viewable() {
        for name in [
            ".bashrc",
            ".zshrc",
            ".profile",
            ".bash_profile",
            ".bash_aliases",
            "GNUmakefile",
            "justfile",
            "tool.pyw",
            "stub.pyi",
            "settings.jsonc",
        ] {
            assert_ne!(
                crate::highlight::lang_for_name(name),
                crate::highlight::Lang::Unknown
            );
            assert!(is_viewable_text(name), "{name}");
        }
    }

    #[test]
    fn bare_well_known_names_are_text() {
        for name in ["README", "Makefile", "LICENSE", "Dockerfile"] {
            assert!(is_viewable_text(name), "{name}");
        }
        assert!(!is_viewable_text("randomname"));
    }
}
