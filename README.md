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

## Case studies: recent npm supply-chain attacks

Nearly every npm supply-chain attack follows the same kill chain: an attacker
compromises a maintainer account (usually by phishing) or a CI token, ships
malicious code inside an otherwise-normal package, and then — at **install**
time or at **runtime** — (1) reads host secrets and files, (2) exfiltrates them
over the network, and (3) persists or self-propagates.

`npxc` removes the capabilities each stage of that chain depends on. It moves
`npm install` into an **ephemeral, isolated build VM** (so install scripts never
touch your host filesystem, environment, or credentials), and it runs the server
with **no network, no host environment variables, no host filesystem** (only
explicitly-named in-CWD files, read-only), a **read-only root**, **all
capabilities dropped**, and as a **non-root** user. The notes below describe how
that posture maps onto specific, real incidents.

> **Honest scoping.** `npxc` is strongest for the large class of MCP servers
> that do *local* work (parsing, file analysis, format conversion), which should
> run with the default `--network none`. Tools that inherently need the network
> (sending email, calling an API) must opt into `network = "bridge"`, and `npxc`
> cannot stop a tool from misusing the network it was *legitimately granted* —
> it still contains filesystem, credential, and host damage, but covert
> exfiltration over an allowed connection is out of scope. Also note that the
> `npm install` step runs in an isolated VM but *does* have network (it must, to
> fetch the package); the protection there is that it is sandboxed away from your
> host, not that it is offline.

### chalk, debug, ansi-styles, … — "Qix" maintainer phished (Sep 8, 2025)

The most-downloaded compromise to date: a phishing email (from the look-alike
domain `npmjs.help`) tricked the prolific maintainer *Qix* into resetting 2FA,
and the attacker published malicious versions of ~18 foundational packages
— `chalk`, `debug`, `ansi-styles`, `strip-ansi`, `color-convert`, and more —
with a combined ~2–3 billion weekly downloads. The payload was a browser-side
crypto-clipper that hooked `window.ethereum`/`fetch`/`XMLHttpRequest` to swap
wallet addresses in transactions.

**How `npxc` helps:** this particular payload only activates in a browser, so it
would lie dormant in a Node MCP server. More generally, though, the lesson is
that *any* dependency can be silently replaced — and had the same maintainer
compromise shipped a Node-side stealer (the usual case below), `npxc`'s
no-network, no-host-secrets sandbox would have left it nothing to steal and
nowhere to send it.

### "Shai-Hulud" worm — `@ctrl/tinycolor` + ~500 packages (Sep 2025)

A self-replicating worm: on install, the payload downloaded TruffleHog, scanned
the filesystem and environment for secrets (`NPM_TOKEN`, `GITHUB_TOKEN`,
`AWS_ACCESS_KEY_ID`, …), probed cloud-metadata endpoints (`169.254.169.254`),
validated the stolen npm token, then used it to **trojanize and republish other
packages the victim owned** — propagating automatically — and exfiltrated
findings to a webhook and to attacker-created public GitHub repositories.

**How `npxc` helps:** the worm's entire premise is harvesting host/CI
credentials and republishing with a stolen token. Under `npxc` the install runs
in a throwaway VM with **no `~/.npmrc`, no host environment, no `~/.ssh`, no host
cloud role**, so the credential sweep comes up empty; and the runtime server has
**no network and no npm token**, so the self-propagation step (which must read a
token and publish over the network) simply cannot run.

### Nx "s1ngularity" — AI-assisted secret theft (Aug 26, 2025)

A leaked CI publish token was used to ship malicious `nx` versions whose
postinstall script harvested GitHub/npm tokens, SSH keys, `.env` files, and
crypto wallets — even abusing locally-installed AI CLIs (`claude`, `gemini`,
`q`) to enumerate sensitive files — then exfiltrated everything to public
`s1ngularity-repository` repos under the victim's account. It also appended
`sudo shutdown -h 0` to `~/.bashrc`/`~/.zshrc`, bricking interactive shells.

**How `npxc` helps:** every target is on the *host* — `~/.npmrc`, `~/.ssh`,
`.env`, wallet files, the host's AI CLIs, and the host shell RC files. The
sandbox exposes none of them: no host filesystem, no host environment, no host
binaries, and a read-only mount, so the harvest finds nothing and the
shell-RC persistence/DoS can't touch the host. With no network at runtime, the
exfiltration to GitHub also fails.

### `postmark-mcp` — a malicious **MCP server** (Sep 2025)

The most on-the-nose example for `npxc`: a trojanized npm package that posed as a
Postmark email MCP server and silently **BCC'd every email it sent to an
attacker-controlled address**. Because it was an MCP server, it ran with whatever
trust the host gave it and exfiltrated mail in the normal flow of doing its job.

**How `npxc` helps (and its limit):** most MCP servers do local work and should
run with the default `--network none`, which turns "silently exfiltrates" into
"cannot reach any network at all." An email sender, however, genuinely needs the
network, so you would run it with `network = "bridge"` — and there `npxc` cannot
stop a BCC over the connection the tool was granted. What it still buys you: the
network is **off by default and opt-in per package**, and even when enabled the
server has no access to your host filesystem, credentials, or other tools'
data — so the blast radius is confined to the one capability you deliberately
granted.

### Earlier incidents, same shape

- **`@solana/web3.js` (Dec 2024)** — a phished maintainer pushed versions
  `1.95.6`/`1.95.7` that stole private keys to drain wallets. A key read and
  shipped to the attacker is exactly what the no-network sandbox blocks.
- **`ua-parser-js` (2021)** — account hijack delivered a cryptominer and a
  credential stealer. No network → no mining pool and no exfiltration; CPU caps
  and an ephemeral container limit the rest.
- **`node-ipc` "protestware" (2022)** — wiped/overwrote files for users in
  certain countries, geolocated by IP. With only read-only, in-CWD files
  visible, there is nothing for it to destroy.
- **`event-stream` (2018)** — a malicious transitive dependency targeted a
  specific wallet app's funds. Scoping plus no network removes both the data and
  the exfiltration path.

---

## License

MIT
