# npxc

[![crates.io](https://img.shields.io/crates/v/npxc.svg)](https://crates.io/crates/npxc)
[![docs.rs](https://docs.rs/npxc/badge.svg)](https://docs.rs/npxc)
[![CI](https://github.com/tomchuk/npxc/actions/workflows/ci.yml/badge.svg)](https://github.com/tomchuk/npxc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/crates/l/npxc.svg)](./LICENSE)

Sandboxed npm execution for MCP servers.

Runs Node.js / npm-based [Model Context Protocol](https://modelcontextprotocol.io) servers
inside an isolated Linux VM via [Apple `container`](https://github.com/apple/container),
with dynamic per-request filesystem scoping to the host process's working
directory and optional allowlist-filtered network egress.

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

- **macOS** (Apple Silicon, M-series chip required)
- **Apple `container`** CLI installed ([releases](https://github.com/apple/container/releases)).
  The runtime flags and isolation guarantees below were verified against
  `container` **0.12.3**.
- **Rust toolchain** >= 1.87 (only required to build from source)

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
an MCP server as a command works unchanged; just replace `npx` with `npxc`:

```sh
# Before
npx -y @scope/package-name

# After: same interface, sandboxed
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
actually runs inside an isolated VM with no network access by default and
filesystem access scoped to the current working directory. To grant a server
filtered outbound access, see [Network egress](#network-egress).

### Live example

The repo includes `examples/mcp_probe.rs`, an interactive probe that runs
three scenarios against `@sylphx/pdf-reader-mcp`:

1. Probe: `initialize` + `tools/list`
2. Read PDF: `tools/call` with a local file (must be within CWD)
3. Scope test: attempt to read `/etc/passwd`, expect a `-32602` rejection

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
| `npxc list` | List cached `npxc/...` images |
| `npxc clean <pkg-spec>` | Remove a specific cached image |
| `npxc clean --all` | Remove all cached package images |
| `npxc inspect <pkg-spec>` | Print resolved config, image tag, env grant sheet, mount plan, and egress allow list |
| `npxc doctor` | Check prerequisites and configure the container system |

### Flags

```
--config <path>     Alternate config file (default: ~/.config/npxc/npxc.toml)
--cwd <path>        Override the CWD scope (default: process working directory)
--no-isolate        Disable path scoping; mount CWD read-only instead (warns loudly)
--log-level <lvl>   trace | debug | info | warn | error  (default: warn; to stderr only)
                    Also accepts target directives, e.g. "npxc::egress=info".
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
`@sylphx/pdf-reader-mcp` becomes `sylphx-pdf-reader-mcp.toml`.

### Global config: `npxc.toml`

```toml
[defaults]
node_image    = "node:lts-slim"   # base image for built images
container_cli = "container"       # CLI name or path
network       = "none"            # default egress; per-package [network] enables filtering
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

### Per-package config: `packages/<name>.toml`

```toml
package = "@scope/my-mcp-server"
version = "1.2.3"         # pinned; "latest" is allowed but discouraged

# -- Environment ---------------------------------------------------------------

# Literal values injected as environment variables (non-secret config).
[env]
NODE_OPTIONS = "--max-old-space-size=512"

# Names of host env vars forwarded into the container.
# Only the *name* lives in config; the value is read from npxc's own
# environment at launch time and is never written to disk.
# The container sees only the variables you list here, not the full host env.
env_passthrough = ["OPENAI_API_KEY", "GITHUB_TOKEN"]

# -- Network -------------------------------------------------------------------

# Outbound access for the server. See the "Network egress" section below.
# mode: "none" (default) | "open" | "allowlist".
[network]
mode  = "allowlist"
allow = [
  "api.anthropic.com:443",
  "registry.npmjs.org:443",
]

# -- Storage -------------------------------------------------------------------

# Mount a per-package persistent host directory read-write at /data.
# The host directory is created at:
#   ~/.local/share/npxc/packages/<sanitized-name>/   (honors $XDG_DATA_HOME)
# Use this for servers that need to maintain state across sessions
# (e.g. server-memory, SQLite-backed servers).
[storage]
persist = true

# -- Mounts --------------------------------------------------------------------

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

# -- Path identification -------------------------------------------------------

# Declare which arguments are filesystem paths, keyed by tool name.
# "*" applies to all tools.
[path_arguments]
"*"             = ["path", "file", "filename", "input"]
"read_pdf"      = ["path"]
"extract_pages" = ["path"]

# Declare arguments that must never be treated as paths (false-positive suppression).
[non_path_arguments]
"*" = ["url", "query", "pattern"]

# -- Runtime overrides ---------------------------------------------------------

[runtime]
memory = "1g"
```

### Inspecting the resolved plan

`npxc inspect <pkg-spec>` prints everything that will be passed to the
container at launch; useful for auditing before running:

```
package:         @scope/my-mcp-server
version:         1.2.3
image_tag:       npxc/scope-my-mcp-server:1.2.3
network:         allowlist (2 rule(s))
  allow:         api.anthropic.com:443
  allow:         registry.npmjs.org:443
memory:          1g
env:             ["NODE_OPTIONS"]
env_passthrough: ["OPENAI_API_KEY", "GITHUB_TOKEN"]
storage:         persist → /data (rw)
mount:           /Users/me/project → /project (ro)
```

---

## Network egress

By default a sandboxed server has no network at all. When a server legitimately
needs outbound access, npxc can give it transparent, allowlist-filtered egress
that the package cannot bypass, even as root inside the container.

Egress is selected per package with the `[network]` table:

```toml
[network]
mode  = "allowlist"          # "none" (default) | "open" | "allowlist"
allow = [
  "api.anthropic.com:443",
  "api.openai.com:443",
  "registry.npmjs.org:443",
  "10.0.0.5/32:5432",        # a fixed internal host, by IP and port
]
```

### Modes

| Mode | Behavior |
|---|---|
| `none` | No network interface (the default). |
| `open` | An unfiltered NAT network. Full outbound access; an escape hatch for debugging. |
| `allowlist` | Default-deny egress filtered by npxc. Only destinations in `allow` are reachable. |

### How allowlist mode works

In allowlist mode npxc puts the container on a per-session host-only network
(`container ... --internal`) that has no NAT route to the internet, so the guest
cannot reach anything directly. Inside the container a userspace WireGuard
interface routes all egress to npxc, which terminates the tunnel in-process,
decrypts the traffic, applies the allowlist, and forwards only allowed flows out
through ordinary host sockets.

The no-NAT property is enforced by Apple `container`'s privileged networking
helper, not by anything inside the guest. So the filter is unbypassable: a root
process in the container cannot grant itself a route around npxc, and tearing
down the tunnel just leaves it with no internet at all.

This needs no host root and no changes to Apple `container`. WireGuard runs in
userspace on both ends (the guest VM kernel has no WireGuard module), and npxc
forwards allowed flows with normal host sockets.

### Allow rules

Each entry in `allow` is a destination with an optional port:

- `host:port` matches a hostname (from the TLS SNI on port 443 or the HTTP Host
  header on port 80) and a port, e.g. `api.anthropic.com:443`.
- `host` with no port matches that hostname on any port.
- `cidr:port` or `ip:port` matches by destination address, e.g.
  `10.0.0.0/24:5432` or `10.0.0.5:5432`. A bare IP is a host route.
- IPv6 with a port uses brackets, e.g. `[2001:db8::1]:443`.

An empty `allow` list denies everything. Filtering is by destination: a hostname
rule matches the connection's SNI/Host, an address rule matches its resolved IP,
and the two are independent.

Additional protections in allowlist mode:

- **DNS pinning.** npxc answers the container's DNS queries itself, returning
  records only for allowlisted names and `NXDOMAIN` for anything else. This is
  defense in depth; connect-time SNI/IP filtering is the actual boundary, so
  resolving a name by other means (including DNS over HTTPS to an allowlisted
  resolver) still cannot reach a destination that is not allowlisted.
- **QUIC blocked.** UDP port 443 is denied, so clients fall back to TLS over
  TCP, which npxc filters by SNI.
- **IPv4 and IPv6** are both carried through the tunnel.

### Capabilities

Allowlist mode is the one case where the container runs with capabilities beyond
`--cap-drop ALL`. The entrypoint needs `NET_ADMIN` to bring up the WireGuard
interface and `SETUID`/`SETGID` to drop from root to the `node` user after
setup. These are used only by the trusted entrypoint; the server process ends up
unprivileged. None of them let the guest escape the no-NAT floor, so egress
stays filtered.

### Observability

Every egress decision is logged under the `npxc::egress` target: allowed flows
at `info`, denied flows at `warn`, each with the protocol, destination, and any
peeked hostname. To watch the egress decision stream:

```sh
NPXC_LOG=npxc::egress=info npxc @scope/my-mcp-server
```

`npxc inspect <pkg-spec>` prints the resolved allow list before you run.

---

## Security model

### What `npxc` protects against

- **Malicious package code.** Runs inside an Apple `container` Linux VM with no
  network by default, a read-only root filesystem (`--read-only`, with only a
  `tmpfs` at `/tmp`), every Linux capability dropped (`--cap-drop ALL`), no
  `npm`/`npx` at runtime, and a non-root server process (the entrypoint drops to
  the `node` user after any privileged setup). Allowlist mode adds back only the
  three capabilities the tunnel entrypoint needs; see [Network egress](#network-egress).
- **Broad filesystem access.** The container's `/workspace` is populated
  dynamically: only files explicitly named in MCP tool calls (and only if they
  resolve within the host CWD) are ever visible to the package. Any additional
  mounts must be declared explicitly in the package config and are validated
  within the same CWD scope.
- **Credential theft.** The container inherits no host environment by default.
  `env_passthrough` variables are opt-in per package and per name; the full host
  environment is never exposed.
- **Network exfiltration.** By default the container has no network interface.
  When outbound access is needed, allowlist mode gives the server default-deny
  egress that npxc filters by destination and that cannot be bypassed from
  inside the container, not even by a root process.
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
- **Misuse of an allowed destination.** A host you put on the allowlist (or any
  host in `open` mode) can receive whatever the package sends it. npxc controls
  which destinations are reachable, not what is sent to them.

### npm supply-chain attacks and this sandbox

Most npm supply-chain attacks follow the same pattern: a compromised package
reads host secrets and environment variables, then exfiltrates them over the
network or persists them somewhere on the host. `npxc` removes the capabilities
each stage depends on.

| Incident | Kill-chain stage blocked |
|---|---|
| **Qix maintainer phish** (chalk, debug, ~18 packages, Sep 2025) | No host env or network at runtime, so the payload has nowhere to send stolen data |
| **"Shai-Hulud" worm** (`@ctrl/tinycolor`, ~500 packages, Sep 2025) | No `~/.npmrc`, no host env, no network by default, so the credential sweep finds nothing and the self-propagation step cannot run |
| **Nx "s1ngularity"** (Aug 2025) | No host filesystem, no host env, no host binaries, so harvest targets are unreachable and `~/.bashrc` persistence cannot touch the host |
| **`postmark-mcp`** (Sep 2025) | No network by default; when enabled, allowlist mode limits egress to named destinations and filters the rest |
| **`@solana/web3.js`** (Dec 2024) | Private key read blocked: no host filesystem, no host env |
| **`ua-parser-js`** (2021) | No network by default, so no mining pool; no host files, so no credential stealer |
| **`node-ipc` protestware** (2022) | Read-only in-CWD mounts only, so there is nothing to overwrite |
| **`event-stream`** (2018) | No host filesystem and no network, so data and exfiltration path are both missing |

**Honest limits.** `npxc` is strongest for servers that do local work and run
with the default of no network. Servers that genuinely need the network can use
allowlist mode, which restricts egress to named destinations but cannot stop
misuse of a destination that was legitimately allowed. The `npm install` step
runs in an isolated VM but does have network access (necessary to fetch the
package); the protection there is isolation from the host, not being offline.

---

## License

MIT
