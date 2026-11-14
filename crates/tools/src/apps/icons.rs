//! The icon of an application, found in the system's folders only and
//! turned into pixels away from the interface: an icon comes from its
//! publisher, so it is read with bounds (size, dimensions), never when the
//! file itself is a link, and an SVG never reads another file
//! (`<image href>`).

use std::io;
use std::path::{Component, Path, PathBuf};

/// Far beyond any icon.
const MOST_BYTES: usize = 1024 * 1024;
/// The widest and tallest PNG decoded.
const MOST_SIDE: u32 = 512;
/// The side an SVG is drawn at: twice the largest icon shown (the theme's
/// `app-icon-large`), for a screen at double scale.
pub const DRAWN_SIDE: u16 = 88;
/// The sizes of the `hicolor` theme looked into, after `scalable`: the
/// smallest as large as [`DRAWN_SIDE`] first, then the larger, then the
/// smaller, so that no more is decoded than is shown.
const SIZES: [&str; 10] = [
    "96x96", "128x128", "256x256", "512x512", "64x64", "48x48", "32x32", "24x24", "22x22", "16x16",
];
/// Where the system's shared files are.
const SHARE: &str = "/usr/share";
/// Where snapd keeps its state, and mounts the snaps.
const SNAPD: &str = "/var/lib/snapd";
const SNAPS: &str = "/snap";
/// The start of a gzip stream: an SVGZ, never read.
const GZIP: [u8; 2] = [0x1f, 0x8b];

/// An icon, as pixels: rows of RGBA, 8 bits each.
#[derive(Clone, PartialEq, Eq)]
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The colour already multiplied by its alpha (an SVG drawn).
    pub premultiplied: bool,
}

impl std::fmt::Debug for IconPixels {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IconPixels")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Where icons are looked for.
#[derive(Debug, Clone)]
pub struct IconRoots {
    /// Folders holding a `hicolor` theme.
    pub themes: Vec<PathBuf>,
    /// Folders of plain icons (`/usr/share/pixmaps`).
    pub pixmaps: Vec<PathBuf>,
    /// The folders an icon named by its path may be under.
    pub absolute: Vec<PathBuf>,
}

impl IconRoots {
    /// The system's: never under a home folder.
    #[must_use]
    pub fn system() -> Self {
        // A flatpak's icons are looked for in its own deployment: those of
        // the installation's exports are links to them.
        Self {
            themes: vec![
                Path::new(SHARE).join("icons"),
                Path::new(SNAPD).join("desktop/icons"),
            ],
            pixmaps: vec![Path::new(SHARE).join("pixmaps")],
            absolute: [SHARE, SNAPS, SNAPD].map(PathBuf::from).to_vec(),
        }
    }
}

/// The file of the icon a launcher names (`Icon=`), if it is in `roots`.
#[must_use]
pub fn find(icon: &str, roots: &IconRoots) -> Option<PathBuf> {
    let path = Path::new(icon);
    if path.is_absolute() {
        let inside = path
            .components()
            .all(|component| !matches!(component, Component::ParentDir))
            && roots.absolute.iter().any(|root| path.starts_with(root));
        if !inside {
            tracing::debug!(icon, "icon outside the system's folders: not read");
            return None;
        }
        return is_file(path).then(|| path.to_owned());
    }
    if !okapi_platform::names_a_file(icon) {
        tracing::debug!(icon, "icon name refused");
        return None;
    }
    // Some launchers name the file, not the icon.
    let name = icon
        .strip_suffix(".png")
        .or_else(|| icon.strip_suffix(".svg"))
        .unwrap_or(icon);
    let in_themes = roots.themes.iter().flat_map(|theme| {
        let hicolor = theme.join("hicolor");
        std::iter::once(hicolor.join("scalable/apps").join(format!("{name}.svg"))).chain(
            SIZES
                .iter()
                .map(move |size| hicolor.join(size).join("apps").join(format!("{name}.png"))),
        )
    });
    let in_pixmaps = roots.pixmaps.iter().flat_map(|folder| {
        ["png", "svg"].map(|extension| folder.join(format!("{name}.{extension}")))
    });
    let found = in_themes
        .chain(in_pixmaps)
        .find(|candidate| is_file(candidate));
    if found.is_none() {
        tracing::debug!(icon, "icon not found");
    }
    found
}

/// A regular file, not a link.
fn is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Why an icon was not turned into pixels.
#[derive(Debug, thiserror::Error)]
pub enum IconError {
    #[error("cannot be read: {0}")]
    Io(io::Error),
    #[error("a link, a folder, a pipe or a device: not a file")]
    NotAFile,
    #[error("larger than {MOST_BYTES} bytes")]
    TooLarge,
    #[error("{width} x {height} pixels, beyond {MOST_SIDE}")]
    TooWide { width: u32, height: u32 },
    #[error("compressed SVG, never read")]
    Compressed,
    #[error("not a PNG nor an SVG: {0}")]
    Unknown(String),
    #[error("PNG not decoded: {0}")]
    Png(#[from] png::DecodingError),
    #[error("SVG not drawn: {0}")]
    Svg(String),
}

/// The pixels of the icon at `path` (found by [`find`]).
///
/// # Errors
///
/// [`IconError`]: it cannot be read, is too large, or is not a PNG or an
/// SVG that can be drawn.
pub fn load(path: &Path) -> Result<IconPixels, IconError> {
    let loaded = okapi_platform::read_regular_capped(path, MOST_BYTES)
        .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidData => IconError::TooLarge,
            io::ErrorKind::InvalidInput => IconError::NotAFile,
            // A link as the file itself is refused by the system (`ELOOP`).
            _ => IconError::Io(error),
        })
        .and_then(|bytes| {
            if path.extension().is_some_and(|extension| extension == "svg") {
                draw_svg(&bytes)
            } else {
                decode_png(&bytes)
            }
        });
    if let Err(error) = &loaded {
        tracing::debug!(path = %path.display(), %error, "icon not loaded");
    }
    loaded
}

fn decode_png(bytes: &[u8]) -> Result<IconPixels, IconError> {
    let mut decoder = png::Decoder::new(io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width > MOST_SIDE || height > MOST_SIDE {
        return Err(IconError::TooWide { width, height });
    }
    let size = reader
        .output_buffer_size()
        .ok_or(IconError::TooWide { width, height })?;
    let mut buffer = vec![0; size];
    let frame = reader.next_frame(&mut buffer)?;
    buffer.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[red, green, blue]| [*red, *green, *blue, u8::MAX])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[gray, alpha]| [*gray, *gray, *gray, *alpha])
            .collect(),
        png::ColorType::Grayscale => buffer
            .iter()
            .flat_map(|gray| [*gray, *gray, *gray, u8::MAX])
            .collect(),
        png::ColorType::Indexed => {
            return Err(IconError::Unknown("a palette left unexpanded".to_owned()));
        }
    };
    Ok(IconPixels {
        width,
        height,
        rgba,
        premultiplied: false,
    })
}

fn draw_svg(bytes: &[u8]) -> Result<IconPixels, IconError> {
    use resvg::{tiny_skia, usvg};
    if bytes.starts_with(&GZIP) {
        return Err(IconError::Compressed);
    }
    let options = usvg::Options {
        // Never another file, never data inside it: only the drawing.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options)
        .map_err(|error| IconError::Svg(error.to_string()))?;
    let side = u32::from(DRAWN_SIDE);
    let mut pixmap =
        tiny_skia::Pixmap::new(side, side).ok_or_else(|| IconError::Svg("no pixmap".to_owned()))?;
    let drawing = tree.size();
    let drawn = f32::from(DRAWN_SIDE);
    let scale = drawn / drawing.width().max(drawing.height());
    // A drawing that is not square is centred.
    let (left, top) = (
        (drawn - drawing.width() * scale) / 2.0,
        (drawn - drawing.height() * scale) / 2.0,
    );
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale).post_translate(left, top),
        &mut pixmap.as_mut(),
    );
    Ok(IconPixels {
        width: side,
        height: side,
        rgba: pixmap.take(),
        premultiplied: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#;

    fn png(width: u32, height: u32, color: png::ColorType) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        if color == png::ColorType::Indexed {
            encoder.set_palette(vec![10, 20, 30]);
        }
        let channels = match color {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Grayscale | png::ColorType::Indexed => 1,
        };
        // Opaque white, or the palette's only colour.
        let fill = if color == png::ColorType::Indexed {
            0
        } else {
            u8::MAX
        };
        let data = vec![fill; usize::try_from(width * height).unwrap() * channels];
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
        bytes
    }

    fn roots(base: &Path) -> IconRoots {
        IconRoots {
            themes: vec![base.join("icons")],
            pixmaps: vec![base.join("pixmaps")],
            absolute: vec![base.join("share")],
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn the_scalable_icon_wins_then_the_largest_then_the_pixmaps() {
        let base = tempfile::tempdir().unwrap();
        let roots = roots(base.path());
        let hicolor = base.path().join("icons/hicolor");
        write(&base.path().join("pixmaps/app.png"), b"p");
        assert_eq!(
            find("app", &roots),
            Some(base.path().join("pixmaps/app.png"))
        );
        write(&hicolor.join("16x16/apps/app.png"), b"p");
        write(&hicolor.join("128x128/apps/app.png"), b"p");
        assert_eq!(
            find("app.png", &roots),
            Some(hicolor.join("128x128/apps/app.png"))
        );
        write(&hicolor.join("scalable/apps/app.svg"), b"s");
        assert_eq!(
            find("app", &roots),
            Some(hicolor.join("scalable/apps/app.svg"))
        );
        assert_eq!(find("none", &roots), None);
    }

    #[test]
    fn an_icon_outside_the_roots_or_through_a_link_is_never_read() {
        let base = tempfile::tempdir().unwrap();
        let roots = roots(base.path());
        write(&base.path().join("share/app.png"), b"p");
        write(&base.path().join("home/secret.png"), b"p");
        let inside = base.path().join("share/app.png");
        assert_eq!(find(inside.to_str().unwrap(), &roots), Some(inside));
        let outside = base.path().join("home/secret.png");
        assert_eq!(find(outside.to_str().unwrap(), &roots), None);
        let climbing = base.path().join("share/../home/secret.png");
        assert_eq!(find(climbing.to_str().unwrap(), &roots), None);
        let link = base.path().join("share/link.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(find(link.to_str().unwrap(), &roots), None);
        assert_eq!(find("../home/secret", &roots), None);
        assert!(
            matches!(load(&link), Err(IconError::Io(_))),
            "never opened through a link"
        );
    }

    /// A PNG of `color` whose first pixel is `first` (its channels, or its
    /// palette's index) and every other pixel 0.
    fn png_with(width: u32, height: u32, color: png::ColorType, first: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        if color == png::ColorType::Indexed {
            encoder.set_palette(vec![0, 0, 0, 10, 20, 30]);
        }
        let mut data = vec![0; usize::try_from(width * height).unwrap() * first.len()];
        data[..first.len()].copy_from_slice(first);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
        bytes
    }

    #[test]
    fn a_png_of_any_colour_type_becomes_rgba_its_channels_in_their_place() {
        let base = tempfile::tempdir().unwrap();
        for (color, first, rgba) in [
            (
                png::ColorType::Rgba,
                &[10, 20, 30, 40][..],
                [10, 20, 30, 40],
            ),
            (png::ColorType::Rgb, &[10, 20, 30], [10, 20, 30, 255]),
            (
                png::ColorType::GrayscaleAlpha,
                &[40, 200],
                [40, 40, 40, 200],
            ),
            (png::ColorType::Grayscale, &[40], [40, 40, 40, 255]),
            (png::ColorType::Indexed, &[1], [10, 20, 30, 255]),
        ] {
            let path = base.path().join("icon.png");
            write(&path, &png_with(3, 2, color, first));
            let pixels = load(&path).unwrap();
            assert_eq!((pixels.width, pixels.height), (3, 2), "{color:?}");
            assert_eq!(pixels.rgba.len(), 3 * 2 * 4, "{color:?}");
            assert_eq!(pixels.rgba[..4], rgba, "{color:?}");
            assert_ne!(
                pixels.rgba[pixels.rgba.len() - 4..],
                rgba,
                "{color:?}: the first pixel first"
            );
            assert!(!pixels.premultiplied);
        }
    }

    #[test]
    fn an_icon_too_large_too_wide_too_tall_or_not_a_file_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let at = |name: &str, bytes: &[u8]| {
            let path = base.path().join(name);
            write(&path, bytes);
            path
        };
        let gray = png::ColorType::Grayscale;
        assert!(matches!(
            load(&at("wide.png", &png(MOST_SIDE + 1, 1, gray))),
            Err(IconError::TooWide { .. })
        ));
        assert!(matches!(
            load(&at("tall.png", &png(1, MOST_SIDE + 1, gray))),
            Err(IconError::TooWide { .. })
        ));
        assert!(
            load(&at("most.png", &png(MOST_SIDE, MOST_SIDE, gray))).is_ok(),
            "the bound itself"
        );
        assert!(matches!(
            load(&at("heavy.png", &vec![0; MOST_BYTES + 1])),
            Err(IconError::TooLarge)
        ));
        assert!(
            !matches!(
                load(&at("just.png", &vec![0; MOST_BYTES])),
                Err(IconError::TooLarge)
            ),
            "the bound itself is read"
        );
        assert!(matches!(
            load(Path::new("/dev/zero")),
            Err(IconError::NotAFile)
        ));
        assert!(matches!(load(base.path()), Err(IconError::NotAFile)));
    }

    fn citing(href: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="16" height="16"><image width="16" height="16" xlink:href="{href}"/></svg>"#
        )
    }

    /// What resvg draws of `svg` with its own way of reading images: the
    /// witness that a citation would be drawn but for Okapi's.
    fn drawn_by_default(svg: &str) -> bool {
        use resvg::{tiny_skia, usvg};
        let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).unwrap();
        let mut pixmap = tiny_skia::Pixmap::new(16, 16).unwrap();
        resvg::render(
            &tree,
            tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        pixmap.data().iter().any(|byte| *byte != 0)
    }

    #[test]
    fn an_svg_is_drawn_centred_and_never_reads_another_file_nor_data_in_it() {
        let base = tempfile::tempdir().unwrap();
        let plain = base.path().join("plain.svg");
        write(&plain, SVG.as_bytes());
        let drawn = load(&plain).unwrap();
        let side = u32::from(DRAWN_SIDE);
        assert_eq!((drawn.width, drawn.height), (side, side));
        assert!(drawn.premultiplied);
        assert_eq!(
            drawn.rgba[..4],
            [255, 0, 0, 255],
            "drawn to its first corner"
        );
        assert_eq!(
            drawn.rgba[drawn.rgba.len() - 4..],
            [255, 0, 0, 255],
            "and to its last"
        );

        // Twice as wide as tall: a band in the middle, nothing above it.
        let wide = base.path().join("wide.svg");
        write(
            &wide,
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16"><rect width="32" height="16" fill="red"/></svg>"#,
        );
        let drawn = load(&wide).unwrap();
        let row = |y: u32| usize::try_from(y * side * 4).unwrap();
        assert_eq!(drawn.rgba[row(0) + 3], 0, "nothing at the top");
        assert_eq!(drawn.rgba[row(side / 2) + 3], 255, "the band in the middle");

        // A drawing beside it, cited by its path; another, inside it.
        let other = base.path().join("other.svg");
        write(&other, SVG.as_bytes());
        let inline = format!(
            "data:image/svg+xml;base64,{}",
            "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIxNiIgaGVpZ2h0PSIxNiI+PHJlY3Qgd2lkdGg9IjE2IiBoZWlnaHQ9IjE2IiBmaWxsPSJyZWQiLz48L3N2Zz4="
        );
        for href in [other.display().to_string(), inline] {
            let svg = citing(&href);
            assert!(drawn_by_default(&svg), "the witness reads it: {href}");
            let citing_path = base.path().join("citing.svg");
            write(&citing_path, svg.as_bytes());
            let drawn = load(&citing_path).unwrap();
            assert!(
                drawn.rgba.iter().all(|byte| *byte == 0),
                "nothing drawn: {href}"
            );
        }

        let compressed = base.path().join("compressed.svg");
        write(&compressed, &[0x1f, 0x8b, 8, 0]);
        assert!(matches!(load(&compressed), Err(IconError::Compressed)));
    }
}
