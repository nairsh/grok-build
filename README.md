<div align="center">

<h1>
  Atlas (<code>atlas</code>)
</h1>

**Atlas** is a terminal-based AI coding agent, forked from xAI's Grok Build.
It runs as a full-screen TUI that understands your codebase, edits files,
executes shell commands, searches the web, and manages long-running tasks —
interactively, headlessly for scripting/CI, or embedded in editors via the
Agent Client Protocol (ACP). Atlas is an independent fork: it does not share
data, configuration, or infrastructure with a Grok Build install, and it is
not affiliated with or supported by xAI/SpaceXAI.

[Installing the released binary](#installing-the-released-binary) ·
[Building from source](#building-from-source) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Contributing](#contributing) ·
[License](#license)

This repository contains the Rust source for the `atlas` CLI/TUI and its agent
runtime, forked from xAI's SpaceXAI monorepo. See
[`RELEASING-FORK.md`](RELEASING-FORK.md) for how this fork's releases and
auto-updater differ from upstream Grok Build.

A small `SOURCE_REV` file at the root records the full monorepo commit SHA
of the upstream Grok Build tree this fork last synced from.

</div>

---

## Installing the released binary

Prebuilt binaries for this fork are published as GitHub Releases on this
repository (see [`RELEASING-FORK.md`](RELEASING-FORK.md) for how they're
built and how the in-app updater fetches them):

```sh
gh release download --repo <this-repo> --pattern 'grok-*' --output atlas
chmod +x atlas
./atlas --version
```

Release notes for this fork live in-tree per crate (for example
[`crates/codegen/xai-grok-shell/CHANGELOG.md`](crates/codegen/xai-grok-shell/CHANGELOG.md)),
not at upstream's `x.ai/build/changelog`.

## Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/atlas
cargo check -p xai-grok-pager-bin            # fast validation
```

The binary artifact is named `atlas` (this fork renames it from `xai-grok-pager`/
`grok` so it doesn't collide with an upstream `grok` install). Sign in with
`atlas login`, which opens an interactive menu to choose a subscription (xAI,
Claude Pro/Max, ChatGPT Codex) or one of 26 API-key providers. Use
`atlas login <provider>` to jump directly to one, or `/login` from the running
TUI — see the
[providers & connections guide](crates/codegen/xai-grok-pager/docs/user-guide/25-providers-and-connections.md)
and the [authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

Upstream Grok Build's hosted docs at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview) describe most of
the shared behavior, but they document the upstream product, not this fork —
where this fork differs (installer, updater, config paths, auth), defer to
the in-tree guide and [`RELEASING-FORK.md`](RELEASING-FORK.md).

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `atlas` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

## Contributing

> [!NOTE]
> External contributions are not accepted. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

First-party code in this repository is licensed under the **Apache License,
Version 2.0** — see [`LICENSE`](LICENSE).

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
