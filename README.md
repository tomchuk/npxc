# npxc

Sandboxed npm execution for MCP servers.

Runs Node.js / npm-based [Model Context Protocol](https://modelcontextprotocol.io) servers
inside an isolated Linux VM via [Apple `container`](https://github.com/apple/container),
with dynamic per-request filesystem scoping to the host process's working directory.

---

## Installation

### From source

```sh
cargo install --path .
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
npx @scope/package-name

# After — same interface, sandboxed
npxc @scope/package-name
npxc @scope/package-name@1.2.3     # pin a specific version
npxc @scope/package-name -- --arg val
```

In any MCP client config that accepts a `command` + `args`, substitute accordingly:

```json
{
  "command": "npxc",
  "args": ["@scope/package-name"]
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
# Build the binary first, then run the example
cargo build --release && cargo run --release --example mcp_probe

# Or pass a specific PDF path
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
| `npxc inspect <pkg-spec>` | Print resolved config, image tag, mount plan, then exit |
| `npxc doctor` | Check that all prerequisites are present |

### Flags

```
--config <path>     Alternate config file (default: ~/.config/npxc/npxc.toml)
--cwd <path>        Override the CWD scope (default: process working directory)
--no-isolate        Disable path scoping; mount CWD read-only instead (escape hatch, warns loudly)
--log-level <lvl>   trace | debug | info | warn | error  (default: warn; to stderr only)
--dry-run           Resolve config and print the plan, then exit (does not build or run)
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
package = "@sylphx/pdf-reader-mcp"
version = "0.4.2"         # pinned; "latest" is allowed but discouraged

# Declare which arguments are filesystem paths, keyed by tool name.
# "*" applies to all tools.
[path_arguments]
"*"             = ["path", "file", "filename", "input"]
"read_pdf"      = ["path"]
"extract_pages" = ["path"]

# Declare arguments that must never be treated as paths (false-positive suppression).
[non_path_arguments]
"*" = ["url", "query", "pattern"]

# Optional per-package resource overrides.
[runtime]
memory = "1g"
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
  resolve within the host CWD) are ever visible to the package. The mount is
  read-only, so a package cannot write back through the published hard links to
  the host originals. (This is the *default*; `--no-isolate` instead mounts the
  whole CWD read-only.)
- **Network exfiltration.** `--network none` removes all network interfaces
  (verified: outbound connections fail with `ENETUNREACH`).
- **Persistence.** Containers are ephemeral (`--rm`). Nothing survives session end.

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
- **TOCTOU on published files.** The window between `canonicalize` and the hard
  link is not defended (requires a local attacker with write access).

---

## License

MIT
