# Changelog

All notable changes to `npxc` are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.3.0](https://github.com/tomchuk/npxc/compare/v0.2.0...v0.3.0) - 2026-06-01

### Added

- *(tunnel)* phase 3 — DNS pinning, IPv6, QUIC block, egress audit log
- *(tunnel)* phase 2 — egress allowlist enforcement, validated live
- *(tunnel)* phase 1 — image/session integration; egress validated live
- *(tunnel)* phase 1 — ipstack datapath + forwarder
- *(tunnel)* phase 1 — wireguard transport state machine
- *(tunnel)* phase 1 — wireguard key layer + datapath crate stack
- *(network)* phase 0 — per-session isolated network lifecycle

### Fixed

- *(runtime)* reliable teardown, reusable WireGuard base image, README
- *(config)* use ~/.config/npxc and ~/.local/share/npxc on all platforms

### Other

- *(deps)* move to hickory-proto 0.26 (MSRV 1.88)
- *(readme)* document filesystem, storage, and env features
- *(tunnel)* zero-allocation wireguard transport

## [0.2.0](https://github.com/tomchuk/npxc/compare/v0.1.1...v0.2.0) - 2026-05-31

### Added

- env injection, arg forwarding, persistent storage, directory mounts, npx -y compatibility

## [0.1.1](https://github.com/tomchuk/npxc/compare/v0.1.0...v0.1.1) - 2026-05-30

### Other

- add recent npm supply-chain attack case studies to security model
- add crates.io, docs.rs, CI, and license badges
- add crates.io install instructions

## [0.1.0] — 2026-05-30

### Added

- **CLI** (`npxc <pkg-spec> [-- args...]`) as a drop-in replacement for `npx`
  in MCP client configs (Zed, Claude Desktop, etc.).
- Auxiliary subcommands: `build`, `rebuild`, `list`, `clean`, `inspect`,
  `doctor`.
- Global flags: `--config`, `--cwd`, `--no-isolate`, `--log-level`, `--dry-run`.
- **Apple `container` integration**: builds a per-package OCI image tagged
  `npxc/<sanitized-name>:<version>` and runs it in an isolated Linux VM with
  `--network none`, a read-only root filesystem, `--cap-drop ALL`, and a
  non-root user. Verified against `container` 0.12.3.
- **Package-spec validation**: package names and versions are validated
  (npm name grammar; semver or dist-tag for versions) before use, both as a
  correctness check and to keep shell metacharacters out of the image build.
- **`--no-isolate` escape hatch**: skips per-file publication and instead mounts
  the entire host CWD read-only into the container at its real path.
- **Image caching**: first invocation builds the image; subsequent invocations
  reuse it. `npxc rebuild` forces a `--no-cache` rebuild.
- **Dynamic CWD-scoped filesystem**: files named in MCP `tools/call` arguments
  are validated against the host CWD, published via hard-link (with
  cross-filesystem copy fallback) into a per-session tempdir, and mounted
  read-only into the container's `/workspace`.
- **Bidirectional JSON-RPC pipeline**: transparent stdio proxy with two async
  tasks (client→server, server→client) coordinated via `tokio::sync` channels.
- **Path identification** with three composable strategies:
  - `config` — explicit per-tool argument lists from the package config.
  - `schema` — automatic detection from MCP `tools/list` `inputSchema`
    (`format: "path"`, `format: "uri"`, or descriptive text).
  - `heuristic` — value-shape detection (absolute prefix `/`, home prefix `~/`,
    configurable URI prefixes such as `file://`).
- **Non-path suppression** via `non_path_arguments` to prevent false positives
  (e.g. URL arguments that happen to start with `/`).
- **Response translation**: `/workspace/<uuid>/<basename>` paths in server
  responses are rewritten back to the original host paths before the client sees
  them.
- **Configuration system**: TOML-based global config (`npxc.toml`) and
  per-package overrides (`packages/<name>.toml`) with deep-merge semantics.
  Per-package `[runtime]` table allows per-package `memory`, `cpus`, and
  `network` overrides.
- **Version pinning**: resolved versions are pinned in the per-package config
  file; `ensure_version_pinned` is idempotent (no unnecessary file writes).
- **Dockerfile template** with multi-stage build: installs the npm package at
  build time; strips build tools at runtime. No `npm`/`npx` in the final image.
- **`npxc doctor`**: checks that the `container` CLI is available and reports
  its version.
- **Structured logging** via `tracing` / `tracing-subscriber`, gated behind
  `--log-level` (defaults to `warn`; output goes to stderr only).
- Exit codes: `0` normal, `1` config error, `2` runtime unavailable,
  `3` build failed, `4` runtime error, `130` interrupted.
- MIT licence.
