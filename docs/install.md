## Installing & building

### System requirements

| Requirement                 | Details                                            |
| --------------------------- | -------------------------------------------------- |
| Operating systems           | macOS 12+, Ubuntu 20.04+/Debian 10+, or Windows 11 |
| Git (optional, recommended) | 2.23+ for built-in PR helpers                      |
| RAM                         | 4-GB minimum (8-GB recommended)                    |

### DotSlash

The GitHub Release also contains a [DotSlash](https://dotslash-cli.com/) file for the Codex CLI named `codex`. Using a DotSlash file makes it possible to make a lightweight commit to source control to ensure all contributors use the same version of an executable, regardless of what platform they use for development.

### Build from source

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/openai/codex.git
cd codex/codex-rs

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt
rustup component add clippy
# Install helper tools used by the workspace justfile:
cargo install --locked just
# DotSlash fetches pinned development tools such as buildifier on first use.
cargo install --locked dotslash
# Install nextest for the `just test` helper.
cargo install --locked cargo-nextest

# Build Codex.
cargo build

# Launch the TUI with a sample prompt.
cargo run --bin codex -- "explain this codebase to me"

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p <crate-you-touched>

# Run the relevant tests (project-specific is fastest), for example:
just test -p codex-tui
# `just test` runs the test suite via nextest:
just test
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
```

### Windows development package

On native Windows, install the MSVC Rust toolchain, Visual Studio Build Tools
with the **Desktop development with C++** workload and Windows SDK, Python 3.11+
(`python` on `PATH`), PowerShell 7.4+ (`pwsh` on `PATH`), and `just`.
From the repository, build a development package with:

```powershell
just build-cli-package
& "$env:LOCALAPPDATA\Programs\CodexDev\codex-dev.cmd" --version
& "$env:LOCALAPPDATA\Programs\CodexDev\codex-dev.cmd"
```

The recipe uses the native Windows MSVC target and Cargo's `dev` profile. It
packages the CLI, code-mode host, Windows sandbox helpers, and ripgrep under
`%LOCALAPPDATA%\Programs\CodexDev`. Debug symbols remain in Cargo's build output.
You can override the package directory, including paths containing spaces:

```powershell
just build-cli-package 'D:\Dev Tools\CodexDev'
& 'D:\Dev Tools\CodexDev\codex-dev.cmd' --version
```

Add the package root (the directory containing `codex-dev.cmd`) to your `PATH`
to run `codex-dev` from any terminal. Keep its `bin` directory off `PATH` so the
packaged `codex.exe` does not shadow your regular installation.

The Windows launcher always selects the `azure` profile from
`%USERPROFILE%\.codex\azure.config.toml`. It retrieves `AZURE_OPENAI_API_KEY`
through `%USERPROFILE%\.config\codex\read-azure-credential.ps1`, which reads the
`azure-openai-codex` Generic Credential from Windows Credential Manager. It sets
the key only for the launched process and does not print it. If
`AZURE_OPENAI_API_KEY` is already set, the launcher uses that value without
querying Windows Credential Manager. The base provider configuration remains in
`%USERPROFILE%\.codex\config.toml`.

Rebuilding replaces the package directory but leaves the configuration and
credential in the user profile intact. Close the dev CLI before rebuilding so
Windows can replace its executables.

On macOS and Linux, `just build-cli-package` continues to write the package to
`~/.cargo/bin/codex-dev` by default.

## Tracing / verbose logging

Codex is written in Rust, so it honors the `RUST_LOG` environment variable to configure its logging behavior.

The TUI records diagnostics in bounded local stores by default. Set `log_dir` explicitly to enable a plaintext TUI log for a run:

```bash
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
