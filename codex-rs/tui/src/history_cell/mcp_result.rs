//! Width-independent display content retained after an MCP call completes.
//!
//! The wire result can contain multi-megabyte image, audio, or resource bodies. Validate each
//! block once with the MCP model, then retain display text and bounded image previews.

use crate::text_formatting::format_json_compact;
use crate::transcript_image::validate_image;
use base64::Engine;
use codex_protocol::mcp::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::ResourceContents;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;
use tracing::error;

pub(super) const MAX_IMAGES: usize = 8;
pub(super) const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum McpResultKind {
    Standard,
    NodeRepl,
}

#[derive(Debug)]
pub(super) struct McpToolResult {
    pub(super) content: Vec<McpContentBlock>,
    pub(super) is_error: bool,
    pub(super) has_image: bool,
    pub(super) images: Vec<Arc<[u8]>>,
}

#[derive(Debug)]
pub(super) struct McpContentBlock {
    display: McpContentDisplay,
    pub(super) is_image: bool,
    /// Code mode uses a top-level `text` field even on malformed or non-text blocks.
    original_text: Option<String>,
}

#[derive(Debug)]
enum McpContentDisplay {
    Text(String),
    Summary(Cow<'static, str>),
    Json(String),
}

impl McpToolResult {
    /// Consumes a wire result, dropping audio/resource bodies and retaining bounded images.
    ///
    /// Canonical MCP deserialization preserves full text and the exact JSON fallback for malformed
    /// or unknown blocks. Only the first eight image blocks are decoded, sharing a 16 MiB budget.
    pub(super) fn new(result: CallToolResult, kind: McpResultKind) -> Self {
        let mut images = Vec::new();
        let mut image_blocks_remaining = MAX_IMAGES;
        let mut image_bytes_remaining = MAX_IMAGE_BYTES;
        let content = result
            .content
            .into_iter()
            .map(|block| {
                // Deserialize by reference so malformed blocks remain available for the exact
                // JSON fallback. Successful blocks no longer retain their wire representation.
                let parsed = ContentBlock::deserialize(&block);
                let is_image = matches!(&parsed, Ok(ContentBlock::Image(_)));
                let original_text = match (&parsed, kind) {
                    (Ok(ContentBlock::Text(_)), _) | (_, McpResultKind::Standard) => None,
                    (_, McpResultKind::NodeRepl) => block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                };
                let display = match parsed {
                    Ok(ContentBlock::Text(text)) => McpContentDisplay::Text(text.text),
                    Ok(ContentBlock::Image(image)) => {
                        if image_blocks_remaining > 0 {
                            image_blocks_remaining -= 1;
                            if let Some(bytes) =
                                decode_mcp_image(&image.data, &mut image_bytes_remaining)
                            {
                                images.push(bytes);
                            }
                        }
                        McpContentDisplay::Summary("Returned image".into())
                    }
                    Ok(ContentBlock::Audio(_)) => {
                        McpContentDisplay::Summary("<audio content>".into())
                    }
                    Ok(ContentBlock::Resource(resource)) => {
                        let summary = match resource.resource {
                            ResourceContents::TextResourceContents { uri, .. }
                            | ResourceContents::BlobResourceContents { uri, .. } => {
                                format!("embedded resource: {uri}").into()
                            }
                            _ => "<unknown embedded resource>".into(),
                        };
                        McpContentDisplay::Summary(summary)
                    }
                    Ok(ContentBlock::ResourceLink(link)) => {
                        McpContentDisplay::Summary(format!("link: {}", link.uri).into())
                    }
                    Ok(_) | Err(_) => McpContentDisplay::Json(block.to_string()),
                };
                McpContentBlock {
                    display,
                    is_image,
                    original_text,
                }
            })
            .collect();

        Self {
            content,
            is_error: result.is_error.unwrap_or(false),
            has_image: !images.is_empty(),
            images,
        }
    }
}

impl McpContentBlock {
    /// Returns all retained text, with honest markers for media and embedded resource bodies.
    pub(super) fn render_full(&self) -> &str {
        if let Some(text) = &self.original_text {
            return text.trim_end_matches('\n');
        }
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => {
                text.trim_end_matches('\n')
            }
            McpContentDisplay::Summary(summary) => summary,
        }
    }

    /// Returns the untruncated top-level text used by node_repl and cua_repl's compact and
    /// transcript views.
    ///
    /// Valid text blocks reuse their display storage. Only node_repl and cua_repl retain this extra
    /// field on malformed or non-text blocks, where it takes precedence over the usual display
    /// content.
    pub(super) fn text(&self) -> Option<&str> {
        match &self.display {
            McpContentDisplay::Text(text) => Some(text),
            McpContentDisplay::Summary(_) | McpContentDisplay::Json(_) => {
                self.original_text.as_deref()
            }
        }
    }

    /// Formats the complete text or fallback JSON. The caller applies a single preview limit
    /// across all blocks, while transcript and raw output retain the full result.
    pub(super) fn render(&self) -> Cow<'_, str> {
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => {
                format_json_compact(text)
                    .map(Cow::Owned)
                    .unwrap_or(Cow::Borrowed(text))
            }
            McpContentDisplay::Summary(summary) => Cow::Borrowed(summary),
        }
    }
}

/// Validates an MCP image before exposing an image preview in computer activity.
///
/// A header-only check would accept images whose decoder rejects their pixel data. Preserve the
/// existing behavior for invalid base64, unknown formats, corrupt images, and data URLs.
fn decode_mcp_image(data: &str, remaining: &mut usize) -> Option<Arc<[u8]>> {
    let base64_data = if let Some(data_url) = data.strip_prefix("data:") {
        data_url.split_once(',')?.1
    } else {
        data
    };
    // Charge attempts, including corrupt images, before either base64 or pixel allocation.
    let decoded_len = base64_data.len().div_ceil(4).checked_mul(3)?;
    *remaining = remaining.checked_sub(decoded_len)?;
    let raw_data = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| {
            error!("Failed to decode image data: {e}");
            e
        })
        .ok()?;
    validate_image(&raw_data).then(|| raw_data.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_budget_bounds_base64_allocation_and_charges_invalid_attempts() {
        let mut remaining = 3;
        assert!(decode_mcp_image("AAAAAA==", &mut remaining).is_none());
        assert_eq!(remaining, 3);
        assert!(decode_mcp_image("????", &mut remaining).is_none());
        assert_eq!(remaining, 0);
        assert!(decode_mcp_image("AAAA", &mut remaining).is_none());
        assert_eq!(remaining, 0);
    }
}
