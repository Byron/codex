# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Image previews

The fullscreen transcript can display local PNG, JPEG, GIF, WebP, and SVG images
on terminals with supported graphics protocols. Reference a local image with
Markdown, for example `![Latency chart](output/latency.svg)`. Relative paths use
the working directory of the response. Alt text remains available for copying
and Find, and is the fallback when an image cannot be displayed.

Images produced by image generation and MCP tools can also appear as previews.
Expand a tool activity to see its images, or use the detailed transcript. Tool
image bytes work with remote workspaces; local file references require a local
workspace. Kitty previews fit the available width and up to 60 rows. Sixel
previews keep their smaller physical-pixel limits.

For other diagram formats, have the agent run an existing tool to produce a PNG
or SVG, then reference that file. For example, Graphviz can produce an artifact
with `dot -Tsvg graph.dot -o graph.svg`. The TUI does not execute diagram code or
launch converters. SVGs with complex effects can be converted to PNG by a tool.

Previews are static and resource-bounded. Up to eight images are retained per MCP
result; long computer-activity groups retain their most recent images. Excess or
unsupported images keep their text markers. Markdown images inside tables keep
their alt text.

## Mermaid images

Mermaid diagrams automatically render as images in the fullscreen transcript
when the terminal supports the Kitty graphics protocol. No configuration is
required. Unsupported terminals, inline mode, and diagrams that cannot
be rendered as images retain the text fallback. Setting `tui.rendering.mermaid`
to `false` disables both text and image rendering and preserves the diagram source.

Flowchart, sequence, class, state, entity-relationship, and pie diagrams are
supported. Oversized diagrams retain the text fallback. Code-block copying keeps
the original Mermaid source, and Find temporarily reveals the matching text.

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.
