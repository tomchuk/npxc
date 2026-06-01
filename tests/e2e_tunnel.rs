//! Real-runtime end-to-end check for the Phase 2 egress allowlist.
//!
//! This is the full datapath test from `TUNNEL.md` §11: it stands up npxc's
//! actual host-side tunnel ([`tunnel::establish`] + the live `Tunnel`/`Policy`),
//! runs a real container on a per-session host-only (`--internal`) network with
//! userspace `WireGuard`, and asserts that:
//!
//! 1. an **allowed** destination (by IP/port) is reachable, a **denied** one is
//!    not;
//! 2. an **allowed** TLS SNI on 443 connects while a **denied** name does not —
//!    the headline filtering feature;
//! 3. an **allowlisted** name resolves through npxc's in-tunnel resolver while a
//!    non-allowlisted name returns `NXDOMAIN` (DNS pinning);
//! 4. tearing down `wg0` inside the guest yields **no** internet at all (the
//!    unbypassable-floor / bypass test): the `--internal` network has no NAT, so
//!    removing the tunnel removes egress entirely;
//! 5. an allowlisted name reached over **IPv6** completes its TLS handshake
//!    through the tunnel (run only when the host itself has IPv6 egress).
//!
//! It requires the same environment as `e2e_runtime.rs`:
//!
//! - macOS on Apple Silicon,
//! - the `container` CLI on `PATH` (override with `NPXC_CONTAINER_CLI`),
//! - a started system service (`container system start`, or `npxc doctor`),
//! - network access (pulls base images and reaches `example.com`/`1.1.1.1`).
//!
//! Gated behind the `e2e` feature so plain `cargo test` skips it:
//!
//! ```sh
//! cargo test --features e2e --test e2e_tunnel -- --nocapture
//! ```
//!
//! The first run builds a small probe image (a Rust compile of `boringtun-cli`);
//! it is cached as `npxc-e2e-probe:latest` for subsequent runs.

#![cfg(feature = "e2e")]

use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use npxc::config::NetworkPolicy;
use npxc::runtime::ManagedNetwork;
use npxc::tunnel;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

/// Tag for the probe image built (once) by this suite. Bump the version suffix
/// whenever [`PROBE_DOCKERFILE`] changes so a stale cached image is rebuilt.
const PROBE_IMAGE: &str = "npxc-e2e-probe:2";

/// Probe-image Dockerfile: the same userspace-WireGuard tooling as npxc's
/// runtime image (boringtun-cli + `wg`/`ip`), but with a generic `/bin/sh -c`
/// entrypoint so the test can run arbitrary probe scripts. `/wg-up.sh` brings up
/// `wg0` from the injected `NPXC_WG_*` env, exactly as npxc's real entrypoint
/// does. The single-quoted heredoc marker keeps the `$NPXC_*` tokens literal
/// (they're guest-shell expressions, evaluated at run time, not build time).
const PROBE_DOCKERFILE: &str = r#"FROM rust:1-bookworm AS wgbuild
RUN cargo install boringtun-cli --version 0.7.1 --root /usr/local

FROM node:lts-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends wireguard-tools iproute2 \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
COPY --from=wgbuild /usr/local/bin/boringtun-cli /usr/local/bin/boringtun-cli
COPY <<'EOF' /wg-up.sh
#!/bin/sh
set -e
mkdir -p /run/wireguard
boringtun-cli --disable-drop-privileges wg0 >&2
i=0; while [ ! -S /run/wireguard/wg0.sock ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i+1)); done
printf '%s' "$NPXC_WG_PRIVATE_KEY" | wg set wg0 private-key /dev/stdin
wg set wg0 peer "$NPXC_WG_PEER_PUBLIC_KEY" endpoint "$NPXC_WG_ENDPOINT" allowed-ips 0.0.0.0/0,::/0 persistent-keepalive 25
ip address add "$NPXC_WG_ADDRESS" dev wg0
if [ -n "$NPXC_WG_ADDRESS6" ]; then ip -6 address add "$NPXC_WG_ADDRESS6" dev wg0 || true; fi
ip link set wg0 mtu "${NPXC_WG_MTU:-1380}" up
ip route replace default dev wg0
if [ -n "$NPXC_WG_ADDRESS6" ]; then ip -6 route replace default dev wg0 || true; fi
EOF
RUN chmod +x /wg-up.sh
ENTRYPOINT ["/bin/sh", "-c"]
"#;

/// Scenario 1: an HTTP request to an allowed IP must get a response, while one
/// to a different (denied) IP must not. No DNS involved — pure IP/port matching.
///
/// `ipstack` completes the TCP handshake locally before npxc's policy runs, so a
/// bare `connect()` always fires regardless of the decision; a denied flow shows
/// up as a reset with **no bytes exchanged**. The probe therefore sends a real
/// request and keys off whether any response data comes back.
const PROBE_IP: &str = r#"/wg-up.sh >&2 || { echo WG_SETUP_FAILED; exit 0; }
node -e 'const net=require("net");
function t(h,tag){return new Promise(res=>{
  const s=net.connect({host:h,port:80,family:4});
  let got=false,done=false;
  const fin=()=>{if(!done){done=true;console.log(tag,got?"RESPONSE":"NO_RESPONSE");s.destroy();res()}};
  s.setTimeout(8000);
  s.on("connect",()=>s.write("GET / HTTP/1.1\r\nHost: "+h+"\r\nConnection: close\r\n\r\n"));
  s.on("data",()=>{got=true;fin()});
  s.on("error",fin);
  s.on("timeout",fin);
  s.on("close",fin)})}
(async()=>{await t("1.1.1.1","ALLOWED");await t("1.0.0.1","DENIED");process.exit(0)})()'
"#;

/// Scenario 2: a TLS handshake to an allowed SNI must complete and one to a
/// denied SNI must be reset. Both names resolve via the implicitly-allowed DNS
/// resolver; only the SNI distinguishes them.
const PROBE_SNI: &str = r#"/wg-up.sh >&2 || { echo WG_SETUP_FAILED; exit 0; }
node -e 'const tls=require("tls");
function t(host,tag){return new Promise(r=>{const s=tls.connect({host,servername:host,port:443,family:4,rejectUnauthorized:false},()=>{console.log(tag,"TLS_OK");s.destroy();r()});
s.setTimeout(12000);
s.on("error",e=>{console.log(tag,"TLS_ERR",e.code||"err");r()});
s.on("timeout",()=>{console.log(tag,"TLS_TIMEOUT");s.destroy();r()})})}
(async()=>{await t("example.com","ALLOWED");await t("example.org","DENIED");process.exit(0)})()'
"#;

/// Scenario 4: DNS pinning. A query for an allowlisted name must resolve, while
/// a query for any other name must come back `NXDOMAIN` (node reports
/// `ENOTFOUND`) — npxc answers DNS itself, scoped to the allowlist.
const PROBE_DNS: &str = r#"/wg-up.sh >&2 || { echo WG_SETUP_FAILED; exit 0; }
node -e 'const dns=require("dns");
function t(name,tag){return new Promise(r=>{dns.lookup(name,{family:4},(e,addr)=>{
  console.log(tag, e?("NXDOMAIN "+e.code):("RESOLVED "+addr)); r()})})}
(async()=>{await t("example.com","ALLOWED");await t("denied.example.org","DENIED");process.exit(0)})()'
"#;

/// Scenario 5: IPv6 egress. A TLS handshake to an allowlisted name forced over
/// IPv6 must complete — the name's AAAA resolves through the pinned resolver and
/// the v6 flow is carried by the tunnel and SNI-filtered like its v4 twin. Only
/// run when the host itself has IPv6 egress (see [`host_has_ipv6`]).
const PROBE_V6: &str = r#"/wg-up.sh >&2 || { echo WG_SETUP_FAILED; exit 0; }
node -e 'const tls=require("tls");
const s=tls.connect({host:"example.com",servername:"example.com",port:443,family:6,rejectUnauthorized:false},()=>{console.log("V6 TLS_OK");s.destroy();process.exit(0)});
s.setTimeout(12000);
s.on("error",e=>{console.log("V6 TLS_ERR",e.code||"err");process.exit(0)});
s.on("timeout",()=>{console.log("V6 TLS_TIMEOUT");process.exit(0)})'
"#;

/// Scenario 3 (bypass): even with an allow rule that would permit the flow,
/// tearing down `wg0` must leave the guest with no internet — the host-only
/// network has no NAT route, so the tunnel is the only way out.
const PROBE_BYPASS: &str = r#"/wg-up.sh >&2 || { echo WG_SETUP_FAILED; exit 0; }
ip route del default 2>/dev/null || true
ip link set wg0 down 2>/dev/null || true
node -e 'const net=require("net");const s=net.connect({host:"1.1.1.1",port:80,family:4});s.setTimeout(8000);
s.on("connect",()=>{console.log("BYPASS_CONNECT_OK");s.destroy();process.exit(0)});
s.on("error",e=>{console.log("BYPASS_BLOCKED",e.code);process.exit(0)});
s.on("timeout",()=>{console.log("BYPASS_BLOCKED_TIMEOUT");process.exit(0)})'
"#;

/// Resolve the container CLI, honouring `NPXC_CONTAINER_CLI`.
fn container_cli() -> String {
    std::env::var("NPXC_CONTAINER_CLI").unwrap_or_else(|_| "container".to_string())
}

/// Whether the host itself has working IPv6 egress. The v6 scenario is only
/// meaningful (and only able to pass) when npxc can reach v6 destinations.
async fn host_has_ipv6() -> bool {
    // Cloudflare's public v6 resolver, TCP/443.
    let addr: SocketAddr = (
        std::net::Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111),
        443,
    )
        .into();
    matches!(
        timeout(Duration::from_secs(4), TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Ensure the probe image exists, building it once if necessary.
///
/// Returns `false` (so the suite can skip with a message) when the runtime is
/// unavailable or the build fails.
async fn ensure_probe_image(cli: &str) -> bool {
    match Command::new(cli)
        .args(["image", "inspect", PROBE_IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => return true,
        Ok(_) => {} // not present — build it below
        Err(e) => {
            eprintln!("skipping e2e_tunnel: cannot run `{cli}`: {e}");
            return false;
        }
    }

    let tmp = tempfile::tempdir().expect("create build context");
    std::fs::write(tmp.path().join("Dockerfile"), PROBE_DOCKERFILE).expect("write Dockerfile");

    let built = Command::new(cli)
        .args([
            "build",
            "--platform",
            "linux/arm64",
            "-t",
            PROBE_IMAGE,
            "-f",
        ])
        .arg(tmp.path().join("Dockerfile"))
        .arg(tmp.path())
        .stderr(Stdio::inherit())
        .status()
        .await;

    // The BuildKit builder lingers after a build; stop it (best-effort).
    for args in [["builder", "stop"], ["builder", "delete"]] {
        let _ = Command::new(cli)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    match built {
        Ok(status) if status.success() => true,
        _ => {
            eprintln!("skipping e2e_tunnel: probe image build failed");
            false
        }
    }
}

/// Run one probe: establish the host tunnel with `allow`, launch the probe
/// container on `net_name`, and return its combined stdout+stderr. The tunnel is
/// torn down (dropped) before returning, so scenarios don't interfere.
async fn run_scenario(
    cli: &str,
    net_name: &str,
    gateway: IpAddr,
    allow: Vec<String>,
    script: &str,
) -> Option<String> {
    let setup = tunnel::establish(gateway, &allow)
        .await
        .expect("establish tunnel");

    let mut cmd = Command::new(cli);
    cmd.args([
        "run",
        "--rm",
        "--network",
        net_name,
        "--cap-add",
        "NET_ADMIN",
    ]);
    cmd.arg("-v").arg(format!(
        "{}:/etc/resolv.conf:ro",
        setup.resolv_conf.path().display()
    ));
    for (key, value) in &setup.env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg(PROBE_IMAGE).arg(script);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(e) => {
            eprintln!("skipping e2e_tunnel: cannot run probe container: {e}");
            return None;
        }
    };

    // Dropping `setup` aborts the datapath task and deletes the resolv.conf.
    drop(setup);

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(combined)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // a sequence of independent, self-documenting scenarios
async fn allowlist_is_enforced_end_to_end() {
    let cli = container_cli();
    if !ensure_probe_image(&cli).await {
        return;
    }

    // A per-session host-only network (created exactly as npxc does at runtime).
    let net =
        match ManagedNetwork::provision(&NetworkPolicy::Allowlist { allow: vec![] }, &cli).await {
            Ok((_, Some(net))) => net,
            Ok(_) => {
                eprintln!("skipping e2e_tunnel: provision returned no managed network");
                return;
            }
            Err(e) => {
                eprintln!("skipping e2e_tunnel: cannot create --internal network: {e}");
                return;
            }
        };
    let net_name = net.name().to_string();
    let Ok(gateway) = net.gateway.parse::<IpAddr>() else {
        eprintln!(
            "skipping e2e_tunnel: gateway {:?} is not an IP",
            net.gateway
        );
        let _ = net.delete().await;
        return;
    };

    // The v6 scenario only runs (and can only pass) when the host has IPv6
    // egress; otherwise it's skipped so the suite stays green on v4-only hosts.
    let host_v6 = host_has_ipv6().await;

    let mut scenarios = vec![
        ("ip-allow-deny", vec!["1.1.1.1:80".to_string()], PROBE_IP),
        (
            "sni-allow-deny",
            vec!["example.com:443".to_string()],
            PROBE_SNI,
        ),
        (
            "dns-pinning",
            vec!["example.com:443".to_string()],
            PROBE_DNS,
        ),
        (
            "wg0-teardown-bypass",
            vec!["1.1.1.1:80".to_string()],
            PROBE_BYPASS,
        ),
    ];
    if host_v6 {
        scenarios.push(("ipv6-egress", vec!["example.com:443".to_string()], PROBE_V6));
    } else {
        eprintln!("note: host has no IPv6 egress; skipping the ipv6-egress scenario");
    }

    let mut results: Vec<(&str, String)> = Vec::new();
    for (label, allow, script) in scenarios {
        if let Some(out) = run_scenario(&cli, &net_name, gateway, allow, script).await {
            results.push((label, out));
        } else {
            // Runtime hiccup mid-suite: clean up and skip rather than fail.
            let _ = net.delete().await;
            return;
        }
    }

    // Tear the network down before asserting, so a failed assertion can't leak it.
    let _ = net.delete().await;

    for (label, out) in &results {
        eprintln!("=== scenario: {label} ===\n{out}");
    }
    let out = |label: &str| -> &str {
        &results
            .iter()
            .find(|(l, _)| *l == label)
            .expect("scenario was run")
            .1
    };

    // ── IP/port allowlist ────────────────────────────────────────────────
    let ip = out("ip-allow-deny");
    assert!(
        ip.contains("ALLOWED RESPONSE"),
        "allowed IP 1.1.1.1:80 should return an HTTP response through the tunnel; got:\n{ip}"
    );
    assert!(
        !ip.contains("DENIED RESPONSE"),
        "denied IP 1.0.0.1:80 must exchange no data (reset); got:\n{ip}"
    );

    // ── SNI allowlist on 443 ──────────────────────────────────────────
    let sni = out("sni-allow-deny");
    assert!(
        sni.contains("ALLOWED TLS_OK"),
        "allowed SNI example.com:443 should complete its TLS handshake; got:\n{sni}"
    );
    assert!(
        !sni.contains("DENIED TLS_OK"),
        "denied name must not complete a TLS handshake (blocked at DNS or by SNI reset); got:\n{sni}"
    );

    // ── DNS pinning ─────────────────────────────────────────────────────
    let dns = out("dns-pinning");
    assert!(
        dns.contains("ALLOWED RESOLVED"),
        "an allowlisted name must resolve through the in-tunnel resolver; got:\n{dns}"
    );
    assert!(
        dns.contains("DENIED NXDOMAIN"),
        "a non-allowlisted name must return NXDOMAIN; got:\n{dns}"
    );
    assert!(
        !dns.contains("DENIED RESOLVED"),
        "a non-allowlisted name must NOT resolve; got:\n{dns}"
    );

    // ── bypass (wg0 torn down) ──────────────────────────────────────────
    let bypass = out("wg0-teardown-bypass");
    assert!(
        bypass.contains("BYPASS_BLOCKED"),
        "with wg0 torn down the host-only network must have no internet; got:\n{bypass}"
    );
    assert!(
        !bypass.contains("BYPASS_CONNECT_OK"),
        "guest reached the internet without the tunnel — the floor is bypassable; got:\n{bypass}"
    );

    // ── IPv6 egress (only when the host has v6) ──────────────────────────────
    if host_v6 {
        let v6 = out("ipv6-egress");
        assert!(
            v6.contains("V6 TLS_OK"),
            "an allowlisted name over IPv6 should complete its TLS handshake; got:\n{v6}"
        );
    }
}
