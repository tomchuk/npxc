# npxc

[![crates.io](https://img.shields.io/crates/v/npxc.svg)](https://crates.io/crates/npxc)
[![docs.rs](https://docs.rs/npxc/badge.svg)](https://docs.rs/npxc)
[![CI](https://github.com/tomchuk/npxc/actions/workflows/ci.yml/badge.svg)](https://github.com/tomchuk/npxc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/crates/l/npxc.svg)](./LICENSE)

Sandboxed npm execution for MCP servers.

Runs Node.js / npm-based [Model Context Protocol](https://modelcontextprotocol.io) servers
inside an isolated Linux VM via [Apple `container`](https://github.com/apple/container),
with dynamic per-request filesystem scoping to the host process's working directory.

---

## Installation

### From crates.io

```sh
cargo install npxc --locked
```

(`--locked` builds against the published `Cargo.lock` for a reproducible build.)

### From source

```sh
cargo install --path . --locked
```

Or build a release binary:

```sh
cargo build --release
# binary at: ./target/release/npxc
```

---

## Prerequisites

- **macOS** (Apple Silicon — M-series chip required)
- **Apple `container`** CLI installed ([releases](https://github.com/apple/container/releases)).
  The runtime flags and isolation guarantees below were verified against
  `container` **0.12.3**.
- **Rust toolchain** ≥ 1.87 (only required to build from source)

After installing `container`, run:

```sh
npxc doctor
```

This verifies the CLI is on your `PATH` and fully configures the container
system for you:

1. Checks whether the `container` system service is running.
2. If not running, starts it with `container system start --enable-kernel-install`
   (which also installs the default kernel on first run).
3. If the service is already running but no default kernel is configured, runs
   `container system kernel set --recommended` to download and install one.

Running `npxc doctor` once after installation is all that is normally needed.

---

## Usage

### Drop-in replacement for `npx`

`npxc` is a transparent stdio proxy. Any tool or editor that lets you configure
an MCP server as a command works unchanged — just replace `npx` with `npxc`:

```sh
# Before
npx -y @scope/package-name

# After — same interface, sandboxed
npxc @scope/package-name
npxc @scope/package-name@1.2.3          # pin a specific version
npxc @scope/package-name -- --arg val   # args forwarded to the server
```

The leading `-y`/`--yes` that MCP clients commonly emit (from the `npx -y`
convention) is silently absorbed, so configs copied verbatim from `npx` to
`npxc` work without modification:

```json
{
  "command": "npxc",
  "args": ["-y", "@scope/package-name", "--extra-arg"]
}
```

The MCP client sees the server as if it were a local process; the package
actually runs inside an isolated VM with no network access and filesystem access
scoped to the current working directory.

### Live example

The repo includes `examples/mcp_probe.rs`, an interactive probe that runs
three scenarios against `@sylphx/pdf-reader-mcp`:

1. **Probe** — `initialize` + `tools/list`
2. **Read PDF** — `tools/call` with a local file (must be within CWD)
3. **Scope test** — attempt to read `/etc/passwd`, expect a `-32602` rejection

```sh
cargo build --release && cargo run --release --example mcp_probe
cargo build --release && cargo run --release --example mcp_probe /path/to/file.pdf
```

### Subcommands

| Command | Description |
|---|---|
| `npxc <pkg-spec> [-- args...]` | Build (if needed) and run the MCP server |
| `npxc build <pkg-spec>` | Build the image without running |
| `npxc rebuild <pkg-spec>` | Force a `--no-cache` rebuild |
| `npxc list` | List all cached `npxc/…` images |
| `npxc clean <pkg-spec>` | Remove a specific cached image |
| `npxc clean --all` | Remove all cached images |
| `npxc inspect <pkg-spec>` | Print resolved config, image tag, env grant sheet, and mount plan |
| `npxc doctor` | Check prerequisites and configure the container system |

### Flags

```
--config <path>     Alternate config file (default: ~/.config/npxc/npxc.toml)
--cwd <path>        Override the CWD scope (default: process working directory)
--no-isolate        Disable path scoping; mount CWD read-only instead (warns loudly)
--log-level <lvl>   trace | debug | info | warn | error  (default: warn; to stderr only)
--dry-run           Resolve config and print the plan, then exit
```

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Normal shutdown (client closed stdin) |
| `1` | Configuration or argument error |
| `2` | Container runtime not available |
| `3` | Image build failure |
| `4` | Runtime error (container died unexpectedly) |
| `130` | Interrupted (Ctrl-C) |

---

## Configuration

Configuration files follow XDG conventions. On macOS the default location is
`~/.config/npxc/`.

```
~/.config/npxc/
├── npxc.toml                          # global defaults
└── packages/
    ├── sylphx-pdf-reader-mcp.toml     # per-package overrides
    └── ...
```

Per-package filenames are derived from the npm package name: lowercase,
replace `@` and `/` with `-`, strip a leading `-`.
`@sylphx/pdf-reader-mcp` → `sylphx-pdf-reader-mcp.toml`.

### Global config — `npxc.toml`

```toml
[defaults]
node_image    = "node:lts-slim"   # base image for built images
container_cli = "container"       # CLI name or path
network       = "none"            # "none" | "bridge"
memory        = "512m"
cpus          = "1"
mount_mode    = "ro"              # "ro" (recommended) | "rw"
log_level     = "warn"

[paths]
# Order matters: strategies are tried in sequence; results are unioned.
strategies = ["config", "schema", "heuristic"]

[paths.heuristic]
absolute_prefix = true       # args starting with "/" are treated as paths
home_prefix     = true       # args starting with "~/" are treated as paths
uri_prefix      = ["file://"]
```

### Per-package config — `packages/<name>.toml`

```toml
package = "@scope/my-mcp-server"
version = "1.2.3"         # pinned; "latest" is allowed but discouraged

# ── Environment ───────────────────────────────────────────────────────────────

# Literal values injected as environment variables (non-secret config).
[env]
NODE_OPTIONS = "--max-old-space-size=512"

# Names of host env vars forwarded into the container.
# Only the *name* lives in config — the value is read from npxc's own
# environment at launch time and is never written to disk.
# The container sees only the variables you list here, not the full host env.
env_passthrough = ["OPENAI_API_KEY", "GITHUB_TOKEN"]

# ── Storage ───────────────────────────────────────────────────────────────────

# Mount a per-package persistent host directory read-write at /data.
# The host directory is created at:
#   ~/Library/Application Support/npxc/packages/<sanitized-name>/   (macOS)
# Use this for servers that need to maintain state across sessions
# (e.g. server-memory, SQLite-backed servers).
[storage]
persist = true

# ── Mounts ────────────────────────────────────────────────────────────────────

# Extra filesystem mounts beyond the session workspace.
# Host paths are validated to lie within the CWD scope (same rules as per-file
# publication). Relative paths are resolved against the effective CWD.
[[mounts]]
host      = "."              # "." = the CWD itself
container = "/project"
mode      = "ro"             # "ro" (default) | "rw"

[[mounts]]
host      = "config"         # relative: resolves to <cwd>/config
container = "/app/config"
mode      = "ro"

# ── Path identification ───────────────────────────────────────────────────────

# Declare which arguments are filesystem paths, keyed by tool name.
# "*" applies to all tools.
[path_arguments]
"*"             = ["path", "file", "filename", "input"]
"read_pdf"      = ["path"]
"extract_pages" = ["path"]

# Declare arguments that must never be treated as paths (false-positive suppression).
[non_path_arguments]
"*" = ["url", "query", "pattern"]

# ── Runtime overrides ─────────────────────────────────────────────────────────

[runtime]
memory  = "1g"
network = "none"
```

### Inspecting the resolved plan

`npxc inspect <pkg-spec>` prints everything that will be passed to the
container at launch — useful for auditing before running:

```
package:         @scope/my-mcp-server
version:         1.2.3
image_tag:       npxc/scope-my-mcp-server:1.2.3
network:         none
memory:          1g
env:             ["NODE_OPTIONS"]
env_passthrough: ["OPENAI_API_KEY", "GITHUB_TOKEN"]
storage:         persist → /data (rw)
mount:           /Users/me/project → /project (ro)
```

---

## Security model

### What `npxc` protects against

- **Malicious package code.** Runs inside an Apple `container` Linux VM with
  `--network none`, a read-only root filesystem (`--read-only`, with only a
  `tmpfs` at `/tmp`), every Linux capability dropped (`--cap-drop ALL`), no
  `npm`/`npx` at runtime, and a non-root user (`USER node:node`).
- **Broad filesystem access.** The container's `/workspace` is populated
  dynamically: only files explicitly named in MCP tool calls (and only if they
  resolve within the host CWD) are ever visible to the package. Any additional
  mounts must be declared explicitly in the package config and are validated
  within the same CWD scope.
- **Credential theft.** The container inherits no host environment by default.
  `env_passthrough` variables are opt-in per package and per name; the full host
  environment is never exposed.
- **Network exfiltration.** `--network none` removes all network interfaces.
- **Persistence.** Containers are ephemeral (`--rm`). The only state that
  survives a session is data written to an explicit `[storage] persist = true`
  mount.

> The filesystem boundary is the container mount, not the path heuristics: a
> file that `npxc` fails to identify as a path is simply never published, so it
> stays invisible to the package. Path identification is a usability layer on
> top of a fail-closed boundary.

### What `npxc` does not protect against

- **Stdio exfiltration.** A malicious package can include arbitrary content in
  MCP responses. The proxy does not filter output.
- **LLM-driven enumeration.** An LLM that calls a tool repeatedly to read many
  files under CWD is a behavioral problem outside the proxy's scope.
- **Container / VM escape.** `npxc` trusts Apple `container`'s isolation boundary.
- **Network misuse when enabled.** Tools that legitimately need the network
  (`network = "bridge"`) can misuse the connection they were granted.

### npm supply-chain attacks and this sandbox

Most npm supply-chain attacks follow the same pattern: a compromised package
reads host secrets and environment variables, then exfiltrates them over the
network or persists them somewhere on the host. `npxc` removes the capabilities
each stage depends on.

| Incident | Kill-chain stage blocked |
|---|---|
| **Qix maintainer phish** (chalk, debug, ~18 packages, Sep 2025) | No host env or network at runtime — payload has nowhere to send stolen data |
| **"Shai-Hulud" worm** (`@ctrl/tinycolor`, ~500 packages, Sep 2025) | No `~/.npmrc`, no host env, no network — credential sweep finds nothing; self-propagation step cannot run |
| **Nx "s1ngularity"** (Aug 2025) | No host filesystem, no host env, no host binaries — harvest targets unreachable; `~/.bashrc` persistence/DoS cannot touch the host |
| **`postmark-mcp`** (Sep 2025) | Default `--network none` prevents silent exfiltration; network is opt-in per package |
| **`@solana/web3.js`** (Dec 2024) | Private key read blocked — no host filesystem, no host env |
| **`ua-parser-js`** (2021) | No network → no mining pool; no host files → no credential stealer |
| **`node-ipc` protestware** (2022) | Read-only in-CWD mounts only — nothing to overwrite |
| **`event-stream`** (2018) | No host filesystem, no network — data and exfiltration path both missing |

**Honest limits.** `npxc` is strongest for servers that do local work and run
with the default `--network none`. Tools that genuinely need the network must
opt in, and `npxc` cannot stop misuse of a connection that was legitimately
granted. The `npm install` step runs in an isolated VM but does have network
access (necessary to fetch the package); the protection there is isolation from
the host, not being offline.

---

## License

MIT
