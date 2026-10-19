# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

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
