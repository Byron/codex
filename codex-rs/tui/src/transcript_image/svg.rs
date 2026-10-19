//! SVG decoding shared by generated Mermaid diagrams and image artifacts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use resvg::usvg;

use super::MAX_PIXELS;

const MAX_SVG_BYTES: usize = 1024 * 1024;

pub(super) fn render(
    data: &[u8],
    max_width: u32,
    max_height: u32,
    pixel_scale: f32,
) -> Option<image::RgbaImage> {
    let tree = parse(data)?;
    let size = tree.size();
    let (width, height) = super::fitted_size(
        size.width() * pixel_scale,
        size.height() * pixel_scale,
        max_width,
        max_height,
    )?;
    let scale = (width as f32 / size.width()).min(height as f32 / size.height());
    if !bounded_layers(tree.root(), scale, &mut (MAX_PIXELS as f32)) {
        return None;
    }
    let mut pixels = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixels.as_mut(),
    );
    image::RgbaImage::from_raw(width, height, pixels.take_demultiplied())
}

pub(super) fn parse(data: &[u8]) -> Option<usvg::Tree> {
    parse_with_options(data, &svg_options())
}

fn parse_with_options(data: &[u8], options: &usvg::Options<'_>) -> Option<usvg::Tree> {
    if data.len() > MAX_SVG_BYTES {
        return None;
    }
    let source = std::str::from_utf8(data).ok()?;
    if source.contains("<!ENTITY") {
        return None;
    }
    let document = usvg::roxmltree::Document::parse_with_options(
        source,
        usvg::roxmltree::ParsingOptions {
            // Plotters emit SVG DOCTYPEs; external entities have no resolver, and declarations
            // are rejected above so they cannot expand beyond the encoded input budget.
            allow_dtd: true,
            nodes_limit: 10_000,
            ..Default::default()
        },
    )
    .ok()?;
    if !safe_document(&document) {
        return None;
    }
    if options.fontdb.is_empty() && document.descendants().any(|node| node.has_tag_name("text")) {
        return None;
    }
    let tree = usvg::Tree::from_xmltree(&document, options).ok()?;
    bounded_paths(tree.root(), &mut 1_000_000.0).then_some(tree)
}

fn safe_document(document: &usvg::roxmltree::Document<'_>) -> bool {
    // Match usvg's first-ID and xlink:href precedence before checking reusable content.
    let mut ids = HashMap::new();
    for node in document.descendants() {
        if let Some(id) = node.attribute("id") {
            ids.entry(id).or_insert(node);
        }
    }
    let mut expanded_bytes = document.input_text().len();
    document
        .descendants()
        .filter(usvg::roxmltree::Node::is_element)
        .all(|node| {
            if node.ancestors().take(33).count() > 32 {
                return false;
            }
            if node.ancestors().any(|node| node.has_tag_name("metadata")) {
                return true;
            }
            // shortcut: complex SVG effects need tool-side PNG conversion; keep preview work bounded.
            if !matches!(
                node.tag_name().name(),
                "svg"
                    | "g"
                    | "defs"
                    | "path"
                    | "rect"
                    | "circle"
                    | "ellipse"
                    | "polygon"
                    | "polyline"
                    | "line"
                    | "text"
                    | "tspan"
                    | "a"
                    | "title"
                    | "desc"
                    | "style"
                    | "use"
                    | "clipPath"
                    | "linearGradient"
                    | "radialGradient"
                    | "stop"
            ) {
                return false;
            }
            if node.has_tag_name("use") {
                let Some(target) = node
                    .attribute(("http://www.w3.org/1999/xlink", "href"))
                    .or_else(|| node.attribute("href"))
                    .and_then(|href| href.strip_prefix('#'))
                    .and_then(|id| ids.get(id))
                else {
                    return false;
                };
                // Plotters reuse glyph and marker paths. Reused groups can expand exponentially.
                if !matches!(
                    target.tag_name().name(),
                    "path" | "rect" | "circle" | "ellipse" | "polygon" | "polyline" | "line"
                ) || target.children().any(|node| node.is_element())
                {
                    return false;
                }
                expanded_bytes += target.range().len();
                if expanded_bytes > MAX_SVG_BYTES * 4 {
                    return false;
                }
            }
            true
        })
}

fn bounded_paths(group: &usvg::Group, remaining: &mut f32) -> bool {
    group.children().iter().all(|node| {
        let bounded = match node {
            usvg::Node::Group(group) => bounded_paths(group, remaining),
            usvg::Node::Text(text) => bounded_paths(text.flattened(), remaining),
            usvg::Node::Image(_) => false,
            usvg::Node::Path(path) => {
                let segments = path.data().segments().count() as f32;
                let work = path.stroke().and_then(|stroke| stroke.dasharray()).map_or(
                    segments,
                    |dashes| {
                        let smallest_dash = dashes
                            .iter()
                            .copied()
                            .filter(|length| *length > 0.0)
                            .fold(f32::INFINITY, f32::min);
                        let bounds = path.data().bounds();
                        segments * (bounds.width() + bounds.height()) / smallest_dash
                    },
                );
                *remaining -= work;
                remaining.is_finite() && *remaining >= 0.0
            }
        };
        let mut subroots_bounded = bounded;
        if bounded {
            node.subroots(|root| subroots_bounded &= bounded_paths(root, remaining));
        }
        subroots_bounded
    })
}

pub(crate) fn bounded_layers(group: &usvg::Group, scale: f32, remaining: &mut f32) -> bool {
    if group.should_isolate() {
        let bounds = group.abs_layer_bounding_box();
        let pixels = (bounds.width() * scale + 4.0).ceil() * (bounds.height() * scale + 4.0).ceil();
        if !pixels.is_finite() || pixels > *remaining {
            return false;
        }
        *remaining -= pixels;
    }
    group.children().iter().all(|node| match node {
        usvg::Node::Group(group) => bounded_layers(group, scale, remaining),
        usvg::Node::Text(text) => bounded_layers(text.flattened(), scale, remaining),
        usvg::Node::Path(_) => true,
        usvg::Node::Image(_) => false,
    })
}

pub(crate) fn svg_options() -> usvg::Options<'static> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    usvg::Options {
        font_family: "sans-serif".to_string(),
        fontdb: Arc::clone(FONTS.get_or_init(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })),
        // Image source may contain URLs; rendering must never read their contents.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn chart_svg_supports_reused_paths_clip_paths_css_and_transparency() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="640" height="480">
          <metadata><custom-data xmlns="https://example.org">Plot metadata</custom-data></metadata>
          <style>.series { fill: red; fill-opacity: 0.5; }</style>
          <defs>
            <path id="point" d="M0 0 L100 0 L100 100 L0 100 Z"/>
            <clipPath id="axes"><rect width="500" height="400"/></clipPath>
          </defs>
          <g clip-path="url(#axes)"><use xlink:href="#point" class="series" x="10" y="10"/></g>
        </svg>"##;
        let image = render(source, 800, 960, 1.0).unwrap();
        assert_eq!(image.dimensions(), (640, 480));
        assert_eq!(image.get_pixel(20, 20).0, [255, 0, 0, 128]);
        assert_eq!(image.get_pixel(600, 450).0, [0, 0, 0, 0]);
        assert!(crate::transcript_image::validate_image(source));
        let retina = render(source, 1600, 1920, 2.0).unwrap();
        assert_eq!(retina.dimensions(), (1280, 960));
    }

    #[test]
    fn graphviz_svg_with_standard_doctype_renders() {
        let source = include_bytes!("../../tests/fixtures/graphviz.svg");
        let image = render(source, 800, 960, 1.0).unwrap();
        assert!(image.width() > 300 && image.height() > 50);
        assert!(image.pixels().any(|pixel| pixel.0 == [0, 0, 0, 255]));
    }

    #[test]
    fn svg_doctype_cannot_expand_entities_or_load_local_files() {
        for declaration in [
            r#"<!ENTITY label "expanded text">"#,
            r#"<!ENTITY label SYSTEM "file:///tmp/private.svg">"#,
            r#"<!ENTITY % labels SYSTEM "https://example.org/labels.dtd">"#,
        ] {
            let source = format!(
                r#"<!DOCTYPE svg [{declaration}]><svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><text>&label;</text></svg>"#
            );
            assert!(parse(source.as_bytes()).is_none());
        }
    }

    #[test]
    fn svg_rejects_external_resources_unbounded_effects_and_group_references() {
        for element in [
            r#"<image href="/tmp/private.png"/>"#,
            r#"<image href="https://example.com/private.png"/>"#,
            r#"<image href="data:image/png;base64,AAAA"/>"#,
            r#"<filter id="large" width="1000000" height="1000000"/>"#,
            r#"<pattern id="large" width="1000000" height="1000000"/>"#,
            r##"<g id="a"><use href="#a"/></g><use href="#a"/>"##,
            r##"<g id="a"><path d="M0 0 L1 1"/></g><g id="b"><use href="#a"/><use href="#a"/></g><use href="#b"/>"##,
            r##"<path id="safe" d="M0 0 L1 1"/><g id="group"/><use xmlns:xlink="http://www.w3.org/1999/xlink" href="#safe" xlink:href="#group"/>"##,
            r##"<g id="same"/><path id="same" d="M0 0 L1 1"/><use href="#same"/>"##,
            r#"<use href="https://example.com/plot.svg#chart"/>"#,
            r#"<path d="M0 0 L1000 1000" stroke="black" stroke-dasharray="0.000000001"/>"#,
        ] {
            let source = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">{element}</svg>"#
            );
            assert!(parse(source.as_bytes()).is_none(), "accepted {element}");
        }
        assert!(
            render(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><g opacity="0.5"><rect width="1000000" height="1000000"/></g></svg>"#,
                800,
                960,
                1.0,
            )
            .is_none()
        );
        let deeply_nested = format!("<svg>{}{}</svg>", "<g>".repeat(33), "</g>".repeat(33));
        assert!(parse(deeply_nested.as_bytes()).is_none());
    }

    #[test]
    fn svg_bounds_repeated_path_expansion_before_conversion() {
        let source = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><defs><path id="glyph" d="M0 0 {}"/></defs>{}</svg>"##,
            "L1 1 ".repeat(1024),
            r##"<use href="#glyph"/>"##.repeat(1024),
        );
        assert!(source.len() < MAX_SVG_BYTES);
        assert!(parse(source.as_bytes()).is_none());
    }

    #[test]
    fn svg_without_fonts_preserves_fallback_for_labels_but_accepts_outlined_text() {
        let options = usvg::Options::default();
        assert!(options.fontdb.is_empty());
        assert!(
            parse_with_options(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><text x="10" y="20">Plot label</text></svg>"#,
                &options,
            )
            .is_none()
        );
        assert!(
            parse_with_options(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><path d="M0 0 L10 0 L10 10 Z"/></svg>"#,
                &options,
            )
            .is_some()
        );
    }
}
