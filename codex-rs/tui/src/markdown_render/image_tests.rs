use super::ListSpacing;
use super::local_image_path;
use super::render_markdown_lines_with_width_and_cwd;
use super::render_streaming_markdown_lines_with_width_and_cwd;
use crate::markdown_copy::SelectedLine;
use crate::terminal_hyperlinks::HyperlinkLine;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;

fn image_anchors(lines: &[HyperlinkLine]) -> Vec<(String, usize, Arc<PathBuf>)> {
    lines
        .iter()
        .filter_map(|line| line.source.as_ref())
        .flat_map(|source| {
            source
                .copy
                .iter()
                .flat_map(|copy| &copy.images)
                .filter(|(end, _)| source.range.start < *end && *end <= source.range.end)
                .map(|(end, path)| (source.text.to_string(), *end, Arc::clone(path)))
        })
        .collect()
}

#[test]
fn markdown_image_metadata_preserves_visible_alt_text_and_copy() {
    let cwd = std::env::temp_dir();
    let markdown = "Before ![Revenue **2026**](./charts/revenue.png) after.";
    let lines = render_markdown_lines_with_width_and_cwd(markdown, None, Some(&cwd));
    let visible = lines
        .iter()
        .map(|line| line.line.to_string())
        .collect::<String>();
    assert_eq!(visible, "Before Revenue 2026 after.");
    let anchors = image_anchors(&lines);
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].1, "Before Revenue 2026".len());
    assert_eq!(*anchors[0].2, cwd.join("./charts/revenue.png"));

    let mut selection = Vec::new();
    for line in lines {
        let source = line.source.unwrap();
        let range = source.range.clone();
        SelectedLine::append(&mut selection, source, range, "\n");
    }
    assert_eq!(
        crate::markdown_copy::selection(&selection, &visible).0,
        "Before Revenue **2026** after."
    );
}

#[test]
fn markdown_image_metadata_survives_wrapping_streaming_and_nested_containers() {
    let cwd = std::env::temp_dir();
    let markdown = "> - ![A long chart label with Unicode 数据](plot.png) and ![Next](plot.png)\n";
    for width in [12, 80] {
        for lines in [
            render_markdown_lines_with_width_and_cwd(markdown, Some(width), Some(&cwd)),
            render_streaming_markdown_lines_with_width_and_cwd(
                markdown,
                Some(width),
                Some(&cwd),
                &|_| false,
                ListSpacing::AfterMultiline,
            )
            .lines,
        ] {
            let anchors = image_anchors(&lines);
            assert_eq!(anchors.len(), 2);
            assert_eq!(*anchors[0].2, cwd.join("plot.png"));
            assert_eq!(*anchors[1].2, cwd.join("plot.png"));
            assert!(!Arc::ptr_eq(&anchors[0].2, &anchors[1].2));
            assert!(anchors[0].0[..anchors[0].1].ends_with("Unicode 数据"));
            assert!(anchors[1].0[..anchors[1].1].ends_with("Next"));
        }
    }
}

#[test]
fn markdown_image_metadata_anchors_multiline_and_empty_alt_text() {
    let cwd = std::env::temp_dir();
    let lines = render_markdown_lines_with_width_and_cwd(
        "![First\nsecond](plot.png)\n\n![](empty.png)",
        None,
        Some(&cwd),
    );
    let anchors = image_anchors(&lines);
    assert_eq!(anchors.len(), 2);
    assert_eq!((&anchors[0].0, anchors[0].1), (&"second".to_owned(), 6));
    assert_eq!((&anchors[1].0, anchors[1].1), (&"Image".to_owned(), 5));
}

#[test]
fn markdown_image_metadata_anchors_trailing_alt_line_breaks() {
    let cwd = std::env::temp_dir();
    for markdown in ["![First\n](plot.png)", "![First  \n](plot.png)"] {
        let lines = render_markdown_lines_with_width_and_cwd(markdown, None, Some(&cwd));
        let anchors = image_anchors(&lines);
        assert_eq!(anchors.len(), 1, "{markdown}");
        assert_eq!((&anchors[0].0, anchors[0].1), (&"Image".to_owned(), 5));
        assert!(lines.iter().any(|line| line.line.to_string() == "First"));
    }
}

#[test]
fn markdown_image_metadata_tracks_images_wrapped_in_links() {
    let cwd = std::env::temp_dir();
    for markdown in [
        "Before [![Chart](plot.png)](https://example.com) after",
        "Before [![Chart](plot.png)](./report.md) after",
    ] {
        let lines = render_markdown_lines_with_width_and_cwd(markdown, Some(15), Some(&cwd));
        let anchors = image_anchors(&lines);
        assert_eq!(anchors.len(), 1);
        assert_eq!(&anchors[0].0[..anchors[0].1], "Before Chart");
    }
}

#[test]
fn markdown_image_metadata_ignores_remote_html_code_and_unfinished_references() {
    let cwd = std::env::temp_dir();
    for markdown in [
        "![Remote](https://example.com/plot.png)",
        "![Network](//server/share/plot.png)",
        "![Data](data:image/png;base64,AAAA)",
        "![Incomplete](./plot.png",
        "`![Inline code](./plot.png)`",
        "```markdown\n![Fenced code](./plot.png)\n```",
        "<img src=\"./plot.png\" alt=\"HTML\">",
        "| Image |\n| --- |\n| ![Table](plot.png) |",
    ] {
        let lines = render_markdown_lines_with_width_and_cwd(markdown, None, Some(&cwd));
        assert!(image_anchors(&lines).is_empty(), "{markdown}");
    }
}

#[test]
fn markdown_image_paths_accept_only_native_local_files() {
    let cwd = std::env::temp_dir();
    let absolute = cwd.join("chart with spaces.png");
    let file_url = url::Url::from_file_path(&absolute).unwrap();
    for destination in [absolute.to_str().unwrap(), file_url.as_str()] {
        assert_eq!(local_image_path(destination, None), Some(absolute.clone()));
    }
    assert_eq!(
        local_image_path("chart%20with%20spaces.png", Some(&cwd)),
        Some(absolute)
    );
    for destination in [
        "",
        "https://example.com/chart.png",
        "http://example.com/chart.png",
        "data:image/png;base64,AAAA",
        "file://server/share/chart.png",
        "//server/share/chart.png",
        "\\\\server\\share\\chart.png",
        "%2F%2Fserver/share/chart.png",
        "file://[",
    ] {
        assert_eq!(
            local_image_path(destination, Some(&cwd)),
            None,
            "{destination}"
        );
    }
    assert_eq!(local_image_path("plot.png", None), None);
}

#[test]
fn markdown_image_paths_decode_spaces_in_native_windows_drive_paths() {
    for destination in [
        "C:/charts/chart%20with%20spaces.png",
        r"C:\charts\chart%20with%20spaces.png",
    ] {
        let expected = cfg!(windows).then(|| PathBuf::from(destination.replace("%20", " ")));
        assert_eq!(local_image_path(destination, None), expected);
    }
}
