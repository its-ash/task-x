//! Vector graphics: render an SVG file to a PNG the model can look at, with a lint report.
//!
//! The model writes the SVG itself (it is text) with the edit tools; `render_svg` closes the loop,
//! so a model that sees images can check its drawing and fix it. A model without vision passes
//! `image: false` and works from the report alone. Rendering is local (resvg): nothing leaves the
//! machine, and the SVG may not load other files or URLs.

use super::schema::{int, object};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolResult, parse, tool};
use regex::Regex;
use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, Transform};
use resvg::usvg;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

const MAX_SVG_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_SIZE: u32 = 512;
const MIN_SIZE: u32 = 16;
const MAX_SIZE: u32 = 1024;
/// More distinct colours than this is unusual for a logo or an icon.
const MANY_COLORS: usize = 6;
/// A pixel counts as drawn above this alpha (0–255).
const VISIBLE_ALPHA: u8 = 8;

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "render_svg",
        "Render an SVG file (a logo, icon, or illustration you wrote with create_file) to a PNG and look at it, with a report: size, viewBox, element counts, colours, where the artwork sits on the canvas, and warnings (clipped, empty, text that depends on fonts, external or raster images). Write the SVG first, render it, fix what looks wrong, and render again. Pass image false for the report only, when you cannot see images.",
        Permission::Read,
        || {
            object(
                json!({
                    "path": { "type": "string", "description": "The .svg file" },
                    "size": int("Long side of the PNG in pixels (default 512, 16 to 1024)", 16),
                    "background": { "type": "string", "description": "white (default), black, transparent, or #rrggbb; check a logo on both white and black" },
                    "image": { "type": "boolean", "description": "Return the PNG to look at (default true); false gives the report only" },
                }),
                &["path"],
            )
        },
        render_svg,
    )]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenderInput {
    #[serde(alias = "file")]
    path: String,
    #[serde(default, alias = "width")]
    size: Option<u32>,
    #[serde(default, alias = "bg")]
    background: Option<String>,
    #[serde(default)]
    image: Option<bool>,
}

/// System fonts, loaded once and only for an SVG that has text.
static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

static ROOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<svg\b[^>]*>").expect("regex"));
static VIEWBOX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bviewBox\s*=\s*["'][^"']+["']"#).expect("regex"));
static ELEMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<([A-Za-z][\w:.-]*)").expect("regex"));
static COLOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:fill|stroke|stop-color|flood-color|color)\s*[:=]\s*["']?\s*(#[0-9a-f]{3,8}\b|rgba?\([^)]*\)|[a-z]+)"#).expect("regex")
});
static EXTERNAL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhref\s*=\s*["']\s*(?:https?:|file:|//|[^"'#\s][^"']*\.(?:png|jpe?g|gif|webp|svg)\b)"#).expect("regex"));
static HANDLER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\son[a-z]+\s*="#).expect("regex"));

fn background(raw: Option<&str>) -> Result<Option<Color>, ToolError> {
    let v = raw.unwrap_or("white").trim().to_ascii_lowercase();
    let hex = |s: &str| u8::from_str_radix(s, 16).ok();
    let c = match v.as_str() {
        "white" | "" => Color::WHITE,
        "black" => Color::BLACK,
        "transparent" | "none" => return Ok(None),
        s if s.starts_with('#') && s.len() == 7 => match (hex(&s[1..3]), hex(&s[3..5]), hex(&s[5..7])) {
            (Some(r), Some(g), Some(b)) => Color::from_rgba8(r, g, b, 255),
            _ => return Err(ToolError::invalid(format!("background {v} is not a #rrggbb colour"))),
        },
        s if s.starts_with('#') && s.len() == 4 => match (hex(&s[1..2]), hex(&s[2..3]), hex(&s[3..4])) {
            (Some(r), Some(g), Some(b)) => Color::from_rgba8(r * 17, g * 17, b * 17, 255),
            _ => return Err(ToolError::invalid(format!("background {v} is not a #rgb colour"))),
        },
        _ => return Err(ToolError::invalid(format!("background must be white, black, transparent, or #rrggbb, not {v}"))),
    };
    Ok(Some(c))
}

/// A colour as written in the SVG, in one form: `#rrggbb` for hex, lowercase otherwise.
fn normalize_color(raw: &str) -> Option<String> {
    let c = raw.trim().to_ascii_lowercase();
    if matches!(c.as_str(), "none" | "currentcolor" | "inherit" | "transparent" | "url" | "context") {
        return None;
    }
    if let Some(h) = c.strip_prefix('#') {
        return Some(match h.len() {
            3 | 4 => format!("#{}", h.chars().take(3).flat_map(|ch| [ch, ch]).collect::<String>()),
            6 | 8 => format!("#{}", &h[..6]),
            _ => return None,
        });
    }
    Some(c.split_whitespace().collect())
}

/// Facts about the source that do not need the render.
fn inspect(text: &str) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    let root = ROOT.find(text).map(|m| m.as_str()).unwrap_or_default();
    let has_viewbox = VIEWBOX.is_match(root);
    if !has_viewbox {
        warnings.push("The root <svg> has no viewBox, so it will not scale cleanly; add one, e.g. viewBox=\"0 0 512 512\".".into());
    }
    let mut elements: BTreeMap<String, usize> = BTreeMap::new();
    for c in ELEMENT.captures_iter(text) {
        *elements.entry(c[1].to_owned()).or_default() += 1;
    }
    let colors: BTreeSet<String> = COLOR.captures_iter(text).filter_map(|c| normalize_color(&c[1])).collect();
    if colors.len() > MANY_COLORS {
        warnings.push(format!("{} distinct colours; a logo usually needs 1 to 4.", colors.len()));
    }
    if elements.contains_key("text") {
        warnings.push("It has <text>: the look depends on the fonts installed where it is shown. For a logo, draw the letters as paths, or name a common font family with a generic fallback.".into());
    }
    if elements.contains_key("image") {
        warnings.push("It embeds a raster <image>; a vector logo should be shapes and paths only (raster images are not rendered here).".into());
    }
    if EXTERNAL_REF.is_match(text) {
        warnings.push("It refers to an external file or URL; those are not loaded. Keep everything inside the SVG.".into());
    }
    if elements.contains_key("script") || HANDLER.is_match(text) {
        warnings.push("It has a <script> or an event handler (on…=); remove it, a graphic needs neither.".into());
    }
    if elements.contains_key("foreignObject") {
        warnings.push("<foreignObject> is not rendered by most tools; use SVG shapes.".into());
    }
    let report = json!({
        "viewBox": has_viewbox,
        "elements": elements,
        "colors": colors.into_iter().take(16).collect::<Vec<_>>(),
    });
    (report, warnings)
}

/// The box around the drawn pixels, as `[x, y, width, height]` in pixels; None when nothing shows.
fn content_box(p: &Pixmap) -> Option<[u32; 4]> {
    let (w, h) = (p.width(), p.height());
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (i, px) in p.pixels().iter().enumerate() {
        if px.alpha() > VISIBLE_ALPHA {
            let (x, y) = (i as u32 % w, i as u32 / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    (x0 <= x1 && y0 <= y1 && x1 < w && y1 < h).then(|| [x0, y0, x1 - x0 + 1, y1 - y0 + 1])
}

/// Parse options that never load a file or URL: an `<image>` may only carry its data inline.
fn options(with_fonts: bool) -> usvg::Options<'static> {
    let mut opt = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    if with_fonts {
        opt.fontdb = FONTS.clone();
        opt.font_family = "Arial".into();
    }
    opt
}

fn render_svg(c: &ToolCtx, input: Value) -> ToolResult {
    let p: RenderInput = parse(input)?;
    let (abs, rel) = c.resolve_read(&p.path)?;
    let meta = std::fs::metadata(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if !meta.is_file() {
        return Err(ToolError::invalid(format!("{rel} is not a file")));
    }
    if meta.len() > MAX_SVG_BYTES {
        return Err(ToolError::invalid(format!("{rel} is {} bytes; an SVG can be up to {MAX_SVG_BYTES} bytes here", meta.len())));
    }
    let bytes = std::fs::read(&abs).map_err(|e| ToolError::io(e, &rel))?;
    let text = String::from_utf8(bytes).map_err(|_| ToolError::invalid(format!("{rel} is not UTF-8 text; render_svg takes an .svg file")))?;
    let size = p.size.unwrap_or(DEFAULT_SIZE).clamp(MIN_SIZE, MAX_SIZE);
    let bg = background(p.background.as_deref())?;
    c.check_cancel()?;

    let (mut report, mut warnings) = inspect(&text);
    let opt = options(report["elements"].get("text").is_some());
    let tree = usvg::Tree::from_str(&text, &opt).map_err(|e| ToolError::invalid(format!("{rel} is not a valid SVG: {e}")))?;
    let svg_size = tree.size();
    let scale = size as f32 / svg_size.width().max(svg_size.height());
    let (w, h) = (((svg_size.width() * scale).round() as u32).max(1), ((svg_size.height() * scale).round() as u32).max(1));
    let mut art = Pixmap::new(w, h).ok_or_else(|| ToolError::invalid(format!("cannot render {rel} at {w}×{h}")))?;
    resvg::render(&tree, Transform::from_scale(scale, scale), &mut art.as_mut());
    c.check_cancel()?;

    match content_box(&art) {
        None => warnings.push("Nothing visible was drawn: check the fills, the viewBox, and that shapes lie inside it.".into()),
        Some([x, y, bw, bh]) => {
            if x == 0 || y == 0 || x + bw >= w || y + bh >= h {
                warnings.push("The artwork touches the canvas edge, so it may be clipped; leave a margin (about 5–10% of the viewBox).".into());
            }
            let fill = (bw as f32 / w as f32).max(bh as f32 / h as f32);
            if fill < 0.4 {
                warnings.push(format!("The artwork spans only {:.0}% of the canvas; scale it up or tighten the viewBox.", fill * 100.0));
            }
            let pct = |v: u32, of: u32| (v as f32 * 1000.0 / of as f32).round() / 10.0;
            report["contentBox"] = json!({ "x": pct(x, w), "y": pct(y, h), "width": pct(bw, w), "height": pct(bh, h), "unit": "% of canvas" });
        }
    }
    let png_source = match bg {
        None => art,
        Some(color) => {
            let mut out = Pixmap::new(w, h).ok_or_else(|| ToolError::invalid(format!("cannot render {rel} at {w}×{h}")))?;
            out.fill(color);
            out.draw_pixmap(0, 0, art.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            out
        }
    };
    let obj = report.as_object_mut().expect("report is an object");
    obj.insert("path".into(), json!(rel));
    obj.insert("bytes".into(), json!(meta.len()));
    obj.insert("svgSize".into(), json!({ "width": svg_size.width(), "height": svg_size.height() }));
    obj.insert("rendered".into(), json!({ "width": w, "height": h, "background": p.background.as_deref().unwrap_or("white") }));
    obj.insert("warnings".into(), json!(warnings));
    if p.image != Some(false) {
        let png = png_source.encode_png().map_err(|e| ToolError::invalid(format!("cannot encode the PNG: {e}")))?;
        obj.insert("image".into(), json!(true));
        obj.insert("mediaType".into(), json!("image/png"));
        obj.insert("base64".into(), json!(super::fs::base64(&png)));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="40" fill="#1E40AF"/><rect x="35" y="35" width="30" height="30" fill="#fff"/></svg>"##;

    #[test]
    fn renders_a_png_with_a_report() {
        let c = ctx(&[("logo.svg", LOGO)]);
        let mut r = run(&c, "render_svg", json!({ "path": "logo.svg", "size": 200 })).unwrap();
        assert_eq!((r["rendered"]["width"].as_u64(), r["rendered"]["height"].as_u64()), (Some(200), Some(200)));
        assert_eq!(r["viewBox"], true);
        assert_eq!(r["elements"]["circle"], 1);
        assert_eq!(r["colors"], json!(["#1e40af", "#ffffff"]), "hex colours are normalised");
        assert_eq!(r["contentBox"]["width"].as_f64(), Some(80.0), "the circle spans 80% of the canvas");
        assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");
        let (media, data) = super::super::fs::take_image(&mut r).expect("an image");
        assert_eq!(media, "image/png");
        assert!(data.starts_with("iVBORw0KGgo"), "a PNG signature");
        assert!(r.get("base64").is_none(), "take_image moves the data out");
        std::fs::remove_dir_all(&c.root).ok();
    }

    #[test]
    fn report_only_and_warnings_for_common_logo_mistakes() {
        let bad = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect x="0" y="0" width="100" height="10" fill="red"/><text x="10" y="50">Acme</text><image href="https://example.com/a.png" width="10" height="10"/><g onclick="x()"/></svg>"##;
        let c = ctx(&[("bad.svg", bad)]);
        let r = run(&c, "render_svg", json!({ "path": "bad.svg", "image": false })).unwrap();
        assert!(r.get("base64").is_none() && r.get("image").is_none(), "image false gives the report only");
        let w = r["warnings"].to_string();
        for needle in ["no viewBox", "<text>", "raster <image>", "external file or URL", "event handler", "touches the canvas edge"] {
            assert!(w.contains(needle), "missing warning {needle:?}: {w}");
        }
        std::fs::remove_dir_all(&c.root).ok();
    }

    #[test]
    fn empty_tiny_and_invalid_drawings_are_reported() {
        let empty = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="10" height="10" fill="none"/></svg>"#;
        let tiny = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect x="45" y="45" width="10" height="10"/></svg>"#;
        let c = ctx(&[("empty.svg", empty), ("tiny.svg", tiny), ("broken.svg", "<svg><rect></svg>"), ("a.txt", "x")]);
        let r = run(&c, "render_svg", json!({ "path": "empty.svg", "image": false })).unwrap();
        assert!(r["warnings"].to_string().contains("Nothing visible"), "{r}");
        let r = run(&c, "render_svg", json!({ "path": "tiny.svg", "image": false, "background": "transparent" })).unwrap();
        assert!(r["warnings"].to_string().contains("spans only 10%"), "{r}");
        let e = run(&c, "render_svg", json!({ "path": "broken.svg" })).unwrap_err();
        assert!(e.message.contains("not a valid SVG"), "{}", e.message);
        assert!(run(&c, "render_svg", json!({ "path": "missing.svg" })).is_err());
        let e = run(&c, "render_svg", json!({ "path": "a.txt", "background": "teal" })).unwrap_err();
        assert!(e.message.contains("background must be"), "{}", e.message);
        std::fs::remove_dir_all(&c.root).ok();
    }

    #[test]
    fn local_files_are_never_loaded() {
        let c = ctx(&[("a.txt", "x")]);
        let png = c.root.join("secret.png");
        std::fs::write(&png, Pixmap::new(1, 1).unwrap().encode_png().unwrap()).unwrap();
        let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><image href="{}" width="10" height="10"/></svg>"#, png.display());
        let loaded = usvg::Tree::from_str(&svg, &usvg::Options::default()).unwrap();
        assert!(loaded.root().has_children(), "control: usvg's default resolver reads the file");
        let ours = usvg::Tree::from_str(&svg, &options(false)).unwrap();
        assert!(!ours.root().has_children(), "render_svg's options refuse it");
        std::fs::remove_dir_all(&c.root).ok();
    }

    #[test]
    fn backgrounds_and_colours_parse() {
        assert!(background(None).unwrap().is_some());
        assert!(background(Some("transparent")).unwrap().is_none());
        assert!(background(Some("#0f0")).unwrap().is_some() && background(Some("#00ff00")).unwrap().is_some());
        assert!(background(Some("#zzzzzz")).is_err());
        assert_eq!(normalize_color("#ABC").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_color("none"), None);
        assert_eq!(normalize_color("rgb(1, 2, 3)").as_deref(), Some("rgb(1,2,3)"));
    }
}
