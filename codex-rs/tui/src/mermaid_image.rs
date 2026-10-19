//! Bounded Mermaid rasters for the transcript's existing Kitty image transport.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;

use image::RgbaImage;
use mermaid_rs_renderer::LayoutConfig;
use mermaid_rs_renderer::Theme;
use resvg::tiny_skia;
use resvg::usvg;

use crate::pets::ImageProtocol;
use crate::render::highlight::current_syntax_theme;
use crate::terminal_palette;
use crate::transcript_image::TranscriptImage;
use crate::transcript_image::terminal_cell_size;

const MAX_SOURCE_BYTES: usize = 8 * 1024;
const MAX_NODES: usize = 64;
const MAX_EDGES: usize = 128;
const MAX_ROWS: u32 = 60;
const MAX_PIXELS: u32 = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_CACHE_ENTRIES: usize = 16;
const MAX_CACHE_BYTES: usize = MAX_PIXELS as usize * 4;

#[derive(Clone, Eq, PartialEq)]
struct CacheKey {
    source: String,
    columns: u16,
    protocol: ImageProtocol,
    cell_size: (u32, u32),
    theme_revision: u64,
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
}

#[derive(Default)]
struct RenderCache {
    entries: VecDeque<(CacheKey, Option<Arc<TranscriptImage>>)>,
    bytes: usize,
}

impl RenderCache {
    fn get_or_render(
        &mut self,
        key: CacheKey,
        render: impl FnOnce() -> Option<Arc<TranscriptImage>>,
    ) -> Option<Arc<TranscriptImage>> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_key, _)| *cached_key == key)
            && let Some(entry) = self.entries.remove(index)
        {
            let image = entry.1.clone();
            self.entries.push_back(entry);
            return image;
        }
        let image = render();
        let bytes = image
            .as_ref()
            .map_or(0, |image| image.pixels.as_raw().len());
        while self.entries.len() >= MAX_CACHE_ENTRIES || self.bytes + bytes > MAX_CACHE_BYTES {
            let (_, evicted) = self.entries.pop_front()?;
            self.bytes -= evicted
                .as_ref()
                .map_or(0, |image| image.pixels.as_raw().len());
        }
        self.bytes += bytes;
        self.entries.push_back((key, image.clone()));
        image
    }
}

thread_local! {
    static RENDER_CACHE: RefCell<RenderCache> = RefCell::default();
}

pub(crate) fn render(
    source: &str,
    columns: u16,
    protocol: ImageProtocol,
) -> Option<Arc<TranscriptImage>> {
    if source.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let cell_size = terminal_cell_size().unwrap_or((8, 16));
    let key = CacheKey {
        source: source.to_string(),
        columns,
        protocol,
        cell_size,
        theme_revision: crate::render::highlight::syntax_theme_revision(),
        foreground: terminal_palette::default_fg(),
        background: terminal_palette::default_bg(),
    };
    RENDER_CACHE.with(|cache| {
        cache.borrow_mut().get_or_render(key, || {
            render_with_cell_size(source, columns, protocol, cell_size).map(Arc::new)
        })
    })
}

fn render_with_cell_size(
    source: &str,
    columns: u16,
    protocol: ImageProtocol,
    (cell_width, cell_height): (u32, u32),
) -> Option<TranscriptImage> {
    if columns == 0
        || cell_width == 0
        || cell_height == 0
        || cell_width > 128
        || cell_height > 256
        || !matches!(
            protocol,
            ImageProtocol::Kitty | ImageProtocol::KittyLocalFile
        )
        || !bounded_source(source)
    {
        return None;
    }
    let parsed = mermaid_rs_renderer::parse_mermaid_strict(source).ok()?;
    let graph = &parsed.graph;
    if graph.nodes.len() > MAX_NODES
        || graph.edges.len() > MAX_EDGES
        || graph.subgraphs.len() > 16
        || graph.sequence_frames.len() > 16
        || graph.sequence_boxes.len() > 16
        || graph.sequence_notes.len() + graph.state_notes.len() > MAX_NODES
        || graph.sequence_activations.len() > MAX_EDGES
        || graph
            .sequence_autonumber
            .is_some_and(|start| start.checked_add(graph.edges.len()).is_none())
        || graph.pie_slices.len() > MAX_NODES
        || graph
            .pie_slices
            .iter()
            .any(|slice| !slice.value.is_finite() || slice.value < 0.0)
        || !graph
            .pie_slices
            .iter()
            .map(|slice| slice.value)
            .sum::<f32>()
            .is_finite()
        || graph.nodes.values().any(|node| node.label.len() > 1024)
        || (graph.nodes.is_empty() && graph.pie_slices.is_empty())
        || !safe_graph_styles(graph)
    {
        return None;
    }

    let (theme, background) = theme();
    let config = LayoutConfig {
        node_spacing: 24.0,
        rank_spacing: 40.0,
        max_label_width_chars: 30,
        // Avoid the upstream on-disk font cache; the SVG rasterizer owns font discovery.
        fast_text_metrics: true,
        ..Default::default()
    };
    let layout = mermaid_rs_renderer::compute_layout(graph, &theme, &config);
    if matches!(
        layout.diagram,
        mermaid_rs_renderer::layout::DiagramData::Error(_)
    ) {
        return None;
    }
    let dimensions = mermaid_rs_renderer::measure_svg_dimensions(&layout, &config, None);
    let width = dimensions.width;
    let height = dimensions.height;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let max_width = (u32::from(columns) * cell_width).min(MAX_DIMENSION);
    let max_height = (MAX_ROWS * cell_height).min(MAX_DIMENSION);
    let scale = (cell_height as f32 / 16.0)
        .min(max_width as f32 / width)
        .min(max_height as f32 / height)
        .min((MAX_PIXELS as f32 / (width * height)).sqrt());
    // Keep the source fallback when fitting would make labels less than half a text row tall.
    if theme.font_size * scale < cell_height as f32 / 2.0 {
        return None;
    }
    let pixel_width = (width * scale).ceil() as u32;
    let pixel_height = (height * scale).ceil() as u32;
    let image_columns = pixel_width.div_ceil(cell_width);
    let image_rows = pixel_height.div_ceil(cell_height);
    let pixel_width = image_columns * cell_width;
    let pixel_height = image_rows * cell_height;
    if pixel_width > max_width
        || pixel_height > max_height
        || pixel_width.checked_mul(pixel_height)? > MAX_PIXELS
    {
        return None;
    }
    let svg = mermaid_rs_renderer::render_svg(&layout, &theme, &config);
    if !safe_svg(&svg) {
        return None;
    }
    let options = svg_options();
    if options.fontdb.is_empty() {
        return None;
    }
    let tree = usvg::Tree::from_str(&svg, &options).ok()?;
    if !bounded_layers(tree.root(), scale, &mut (MAX_PIXELS as f32)) {
        return None;
    }
    let mut pixels = tiny_skia::Pixmap::new(pixel_width, pixel_height)?;
    // An opaque background also makes tiny-skia's premultiplied RGBA ordinary RGBA.
    pixels.fill(tiny_skia::Color::from_rgba8(
        background.0,
        background.1,
        background.2,
        255,
    ));
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixels.as_mut(),
    );
    Some(TranscriptImage {
        protocol,
        columns: image_columns as u16,
        rows: image_rows as u16,
        pixels: RgbaImage::from_raw(pixel_width, pixel_height, pixels.take())?,
        cell_width,
        cell_height,
    })
}

fn bounded_source(source: &str) -> bool {
    if source.len() > MAX_SOURCE_BYTES
        || source.split(['\n', ';']).count() > MAX_EDGES
        || source.bytes().filter(|byte| *byte == b'&').count() > MAX_NODES
        || source
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
        || source.contains("%%{")
    {
        return false;
    }
    let header = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"));
    // shortcut: only diagram families with bounded graph layouts, audit others before enabling.
    matches!(
        header.and_then(|line| line
            .split(|ch: char| ch.is_whitespace() || ch == ';')
            .next()),
        Some(
            "flowchart"
                | "graph"
                | "sequenceDiagram"
                | "classDiagram"
                | "stateDiagram"
                | "stateDiagram-v2"
                | "erDiagram"
                | "pie"
        )
    )
}

fn safe_graph_styles(graph: &mermaid_rs_renderer::Graph) -> bool {
    let paints = |values: &[Option<&str>]| {
        values.iter().flatten().all(|value| {
            value.len() <= 128
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || " #(),.%+-".contains(ch))
        })
    };
    let stroke = |width: Option<f32>, dash: Option<&str>| {
        width.is_none_or(|width| width.is_finite() && (0.0..=32.0).contains(&width))
            && dash.is_none_or(|dash| {
                dash == "none"
                    || dash.len() <= 128
                        && dash
                            .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
                            .filter(|part| !part.is_empty())
                            .all(|part| {
                                part.parse::<f32>()
                                    .is_ok_and(|n| (0.5..=4096.0).contains(&n))
                            })
            })
    };
    // Upstream inserts these strings into SVG attributes without escaping them.
    graph
        .node_styles
        .values()
        .chain(graph.subgraph_styles.values())
        .chain(graph.class_defs.values())
        .all(|style| {
            paints(&[
                style.fill.as_deref(),
                style.stroke.as_deref(),
                style.text_color.as_deref(),
                style.line_color.as_deref(),
            ]) && stroke(style.stroke_width, style.stroke_dasharray.as_deref())
        })
        && graph
            .edge_styles
            .values()
            .chain(graph.edge_style_default.iter())
            .all(|style| {
                paints(&[style.stroke.as_deref(), style.label_color.as_deref()])
                    && stroke(style.stroke_width, style.dasharray.as_deref())
            })
        && graph
            .sequence_boxes
            .iter()
            .all(|b| paints(&[b.color.as_deref()]))
}

fn bounded_layers(group: &usvg::Group, scale: f32, remaining: &mut f32) -> bool {
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

fn theme() -> (Theme, (u8, u8, u8)) {
    let syntax_theme = current_syntax_theme();
    let background = terminal_palette::default_bg()
        .or_else(|| {
            syntax_theme
                .settings
                .background
                .map(|color| (color.r, color.g, color.b))
        })
        .unwrap_or((24, 24, 24));
    let mut theme = if crate::color::is_light(background) {
        Theme::modern()
    } else {
        Theme::dark()
    };
    theme.font_family = "sans-serif".to_string();
    theme.font_size = 16.0;
    theme.background = hex(background);
    theme.edge_label_background = theme.background.clone();
    if let Some(foreground) = terminal_palette::default_fg().or_else(|| {
        syntax_theme
            .settings
            .foreground
            .map(|color| (color.r, color.g, color.b))
    }) {
        theme.text_color = hex(foreground);
        theme.primary_text_color = theme.text_color.clone();
    }
    (theme, background)
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn svg_options() -> usvg::Options<'static> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    usvg::Options {
        font_family: "sans-serif".to_string(),
        fontdb: Arc::clone(FONTS.get_or_init(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })),
        // Mermaid source may contain URLs; rendering must never read their contents.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

fn safe_svg(svg: &str) -> bool {
    if svg.len() > 1024 * 1024 {
        return false;
    }
    let Ok(document) = usvg::roxmltree::Document::parse(svg) else {
        return false;
    };
    // No images, CSS, reusable subtrees, or filters that could allocate outside our raster bound.
    document
        .descendants()
        .filter(usvg::roxmltree::Node::is_element)
        .all(|node| {
            node.ancestors().take(33).count() <= 32
                && matches!(
                    node.tag_name().name(),
                    "svg"
                        | "g"
                        | "defs"
                        | "marker"
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
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const CLIENT_SERVER: &str = r#"flowchart LR
    C["Client window"] <-->|"Input, resize, display updates"| S["Zellij server<br/>Terminal state and layouts"]
    S <-->|PTY| A["Shell"]
    S <-->|PTY| B["Editor"]
    S <-->|PTY| D["Build or other job"]"#;

    fn cache_key(columns: u16) -> CacheKey {
        CacheKey {
            source: CLIENT_SERVER.to_string(),
            columns,
            protocol: ImageProtocol::Kitty,
            cell_size: (8, 16),
            theme_revision: 0,
            foreground: None,
            background: None,
        }
    }

    #[test]
    fn cache_remembers_failures_and_evicts_old_entries() {
        let mut cache = RenderCache::default();
        for columns in 1..=MAX_CACHE_ENTRIES as u16 {
            assert!(cache.get_or_render(cache_key(columns), || None).is_none());
        }
        assert!(
            cache
                .get_or_render(cache_key(1), || panic!("cached failure was retried"))
                .is_none()
        );
        cache.get_or_render(cache_key(100), || None);
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert_eq!(cache.entries.front().unwrap().0.columns, 3);
        assert!(cache.entries.iter().any(|(key, _)| key.columns == 1));
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn cache_reuses_pixels_and_bounds_retained_rasters() {
        let image = Arc::new(TranscriptImage {
            protocol: ImageProtocol::Kitty,
            columns: 128,
            rows: 60,
            pixels: RgbaImage::new(1024, 4096),
            cell_width: 8,
            cell_height: 16,
        });
        let mut cache = RenderCache::default();
        cache.get_or_render(cache_key(100), || Some(Arc::clone(&image)));
        let cached = cache
            .get_or_render(cache_key(100), || panic!("raster was rerendered"))
            .unwrap();
        assert!(Arc::ptr_eq(&cached, &image));
        cache.get_or_render(cache_key(101), || Some(Arc::clone(&image)));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        assert_eq!(cache.entries.front().unwrap().0.columns, 101);
    }

    #[test]
    fn client_server_diagram_fits_terminal_with_readable_labels() {
        let image =
            render_with_cell_size(CLIENT_SERVER, 100, ImageProtocol::Kitty, (8, 16)).unwrap();
        assert!(image.columns <= 100);
        assert!(image.rows > 6 && image.rows <= MAX_ROWS as u16);
        assert_eq!(
            image.pixels.width(),
            u32::from(image.columns) * image.cell_width
        );
        assert_eq!(
            image.pixels.height(),
            u32::from(image.rows) * image.cell_height
        );
        assert!(image.pixels.pixels().all(|pixel| pixel[3] == 255));
        assert!(
            image
                .pixels
                .pixels()
                .any(|pixel| pixel != image.pixels.get_pixel(0, 0))
        );
    }

    #[test]
    fn sequence_and_pie_diagrams_render_as_pixels() {
        for source in [
            "sequenceDiagram\nAlice->>Bob: Hello\nBob-->>Alice: Hi",
            "pie title Pets\n\"Cats\" : 3\n\"Dogs\" : 2",
        ] {
            let image =
                render_with_cell_size(source, 100, ImageProtocol::KittyLocalFile, (8, 16)).unwrap();
            assert!(image.columns <= 100 && image.rows <= MAX_ROWS as u16);
        }
    }

    #[test]
    fn malformed_and_excessive_diagrams_keep_the_source_fallback() {
        let too_many_nodes = format!(
            "flowchart TD; {}",
            (0..=MAX_NODES)
                .map(|n| format!("N{n}"))
                .collect::<Vec<_>>()
                .join(" --> ")
        );
        for source in [
            "flowchart LR\nsubgraph Unclosed\nA --> B",
            "sequenceDiagram\nautonumber 18446744073709551615\nA->>B: Hello\nB->>A: Hi",
            "packet-beta\n0-18446744073709551615: \"Huge\"",
            "%%{init: {'theme': 'dark'}}%%\nflowchart LR; A --> B",
            "flowchart LR\nA\nstyle A fill:red\" transform=\"scale(1000000)\" opacity=\"0.5",
            "flowchart LR\nA --> B\nlinkStyle default stroke-dasharray:0.00000001 0.00000001",
            "flowchart LR\nA\nstyle A stroke-width:1000000000",
            &"a".repeat(MAX_SOURCE_BYTES + 1),
            &too_many_nodes,
        ] {
            assert!(
                render_with_cell_size(source, 100, ImageProtocol::Kitty, (8, 16)).is_none(),
                "unexpectedly rendered {source}"
            );
        }
        assert!(render_with_cell_size(CLIENT_SERVER, 1, ImageProtocol::Kitty, (8, 16)).is_none());
        assert!(render_with_cell_size(CLIENT_SERVER, 100, ImageProtocol::Sixel, (8, 16)).is_none());
        assert!(render_with_cell_size(CLIENT_SERVER, 100, ImageProtocol::Kitty, (0, 16)).is_none());
    }

    #[test]
    fn svg_cannot_load_external_resources_or_allocate_filter_surfaces() {
        for element in [
            r#"<image href="/tmp/private.png"/>"#,
            r#"<image href="https://example.com/private.png"/>"#,
            r#"<image href="data:image/png;base64,AAAA"/>"#,
            r#"<filter id="large" width="1000000" height="1000000"/>"#,
            r#"<style>@font-face { src: url('/tmp/private.ttf'); }</style>"#,
            r##"<use href="#recursive" id="recursive"/>"##,
        ] {
            assert!(!safe_svg(&format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg">{element}</svg>"#
            )));
        }
        let options = svg_options();
        let oversized_layer = usvg::Tree::from_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><g opacity="0.5"><rect width="1000000" height="1000000"/></g></svg>"#,
            &options,
        ).unwrap();
        assert!(!bounded_layers(
            oversized_layer.root(),
            1.0,
            &mut (MAX_PIXELS as f32)
        ));
        let deeply_nested = format!("<svg>{}{}</svg>", "<g>".repeat(33), "</g>".repeat(33));
        assert!(!safe_svg(&deeply_nested));
        assert!(
            (options.image_href_resolver.resolve_string)("/tmp/private.png", &options).is_none()
        );
        assert!(
            (options.image_href_resolver.resolve_data)("image/png", Arc::new(vec![0]), &options)
                .is_none()
        );
    }
}
