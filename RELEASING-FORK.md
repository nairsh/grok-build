# Releasing from this fork (nairsh/grok-build)

This fork builds and distributes the Atlas CLI through its own GitHub
Releases, and the in-app auto-updater has been migrated to point at
`nairsh/grok-build` instead of the upstream xAI infrastructure.

## How releases work

1. Bump the crate version in `crates/codegen/xai-grok-pager-bin/Cargo.toml`
   (the compiled-in CLI version follows it unless `ATLAS_VERSION` overrides it).
2. Tag the commit `v<version>` (for example `v0.2.101`) and push the tag.
3. The `Release` workflow (`.github/workflows/release.yml`) builds the
   `atlas` binary with the hardened `release-dist` profile for:
   - `macos-aarch64`, `macos-x86_64`
   - `windows-x86_64`
   (Linux builds are intentionally omitted; commented templates remain in
   the workflow matrix.)
4. It publishes a GitHub Release tagged `v<version>` whose assets are named
   `grok-<version>-<os>-<arch>` — the exact names the auto-updater downloads.

A `workflow_dispatch` run builds the same artifacts without a tag (set
`publish_release` to also publish a release for the current crate version).

## Auto-updater behavior

- `xai_grok_update::version::GH_RELEASE_REPO` is `nairsh/grok-build`; the
  updater lists and downloads releases from this repository only.
- The default installer for unconfigured installs is `gh-release`
  (`crates/codegen/xai-grok-update/src/auto_update.rs`, `get_installer`).
  `internal` (upstream x.ai CDN) and `npm` remain available only when
  explicitly selected via `installer = "..."` in `~/.atlas/config.toml` or
  `ATLAS_INSTALLER`.
- Version checks run `gh release list --repo nairsh/grok-build` and installs
  run `gh release download`, so every machine using auto-update needs the
  GitHub CLI (`gh`) installed and authenticated with read access to this
  repository (`gh auth login`). For public repositories, unauthenticated
  `gh` also works within rate limits.

## Required repository settings

- **Actions enabled** on the fork, with permission to create releases
  (the workflow uses the built-in `GITHUB_TOKEN` with
  `permissions: contents: write` — no extra secrets are required).
- Linux matrix entries are commented out (not needed by this team); ARM
  Linux would additionally require the `ubuntu-22.04-arm` hosted runner,
  which private repositories don't get.

## Known limitations

- The `internal` installer's CDN endpoints (`https://x.ai/cli`) and the npm
  package (`@xai-official/grok`) still point at upstream; they are not used
  unless explicitly configured.
- Release notes/changelogs (`x.ai/cli/changelogs`) are still fetched from
  upstream and may not match fork versions.
- Binaries are unsigned/not notarized; macOS users installing manually may
  need to clear the quarantine attribute (the updater's download path does
  not add one).
