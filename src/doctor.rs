//! `npxc doctor` — prerequisite check and system-setup helpers.
//!
//! All functions print their results directly to stdout and absorb errors
//! internally; the goal is to emit as much diagnostic information as possible
//! even when individual checks fail.  The public entry-point is [`run`].

use std::path::Path;

use tokio::process::Command;

// ── JSON shapes ───────────────────────────────────────────────────────────────

/// Parsed output of `container system status --format json`.
///
/// The command emits a single JSON object.
#[derive(Debug, serde::Deserialize)]
struct SystemStatus {
    /// Service state: `"running"`, `"not running"`, or `"unregistered"`.
    status: String,
    /// Platform app-root directory (where kernels live).
    ///
    /// Present as an empty string when the service is not running.
    #[serde(rename = "appRoot")]
    app_root: Option<String>,
}

/// One entry from the `container system version --format json` array.
///
/// The array always contains a CLI entry (`appName == "container"`) and
/// optionally a server entry (`appName == "apiserver"`) when the daemon
/// responds in time.
#[derive(Debug, serde::Deserialize)]
struct VersionInfo {
    version: String,
    #[serde(rename = "buildType")]
    build_type: Option<String>,
    commit: Option<String>,
    #[serde(rename = "appName")]
    app_name: Option<String>,
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Run all doctor checks against `container_cli`, printing results to stdout.
pub async fn run(container_cli: &str) {
    if !check_cli(container_cli).await {
        return;
    }

    println!();
    println!("container system:");

    let Some(status) = fetch_system_status(container_cli).await else {
        return;
    };

    if ensure_system_running(container_cli, &status).await {
        ensure_kernel(container_cli, &status).await;
    }

    ensure_rosetta_disabled(container_cli).await;
}

// ── CLI check ─────────────────────────────────────────────────────────────────

/// Verify the container CLI is on `PATH` and print its version via
/// `container system version --format json`.
///
/// Returns `false` when the CLI is not found.
async fn check_cli(container_cli: &str) -> bool {
    let Ok(bin_path) = which::which(container_cli) else {
        println!("container CLI:  {container_cli} (NOT FOUND)");
        println!("  Install from: https://github.com/apple/container/releases");
        return false;
    };
    println!("container CLI:  {} (found)", bin_path.display());

    match Command::new(container_cli)
        .args(["system", "version", "--format", "json"])
        .output()
        .await
    {
        Ok(output) => match serde_json::from_slice::<Vec<VersionInfo>>(&output.stdout) {
            Ok(versions) => {
                // Prefer the CLI entry; fall back to first element if the
                // appName field is absent (future-proofing).
                let info = versions
                    .iter()
                    .find(|v| v.app_name.as_deref() == Some("container"))
                    .or_else(|| versions.first());
                match info {
                    Some(v) => {
                        let build = v.build_type.as_deref().unwrap_or("release");
                        let commit = v.commit.as_deref().unwrap_or("");
                        println!("  version:      {} ({build}, {commit})", v.version);
                    }
                    None => println!("  version:      (empty version response)"),
                }
            }
            Err(e) => {
                let raw = String::from_utf8_lossy(&output.stdout);
                println!(
                    "  version:      (JSON parse error: {e}; output: {})",
                    raw.trim()
                );
            }
        },
        Err(e) => println!("  version:      (error running version command: {e})"),
    }

    true
}

// ── System status ─────────────────────────────────────────────────────────────

/// Run `container system status --format json` and return the parsed result.
///
/// The output is captured regardless of exit code — the service being stopped
/// or unregistered is a valid (non-error) state that still produces JSON.
/// Returns `None` and prints an error only when the command cannot be spawned
/// or its stdout is not valid JSON.
async fn fetch_system_status(container_cli: &str) -> Option<SystemStatus> {
    let output = match Command::new(container_cli)
        .args(["system", "status", "--format", "json"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            println!("  status:  (error running status command: {e})");
            return None;
        }
    };

    match serde_json::from_slice::<SystemStatus>(&output.stdout) {
        Ok(s) => Some(s),
        Err(e) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!(
                "  status:  (JSON parse error: {e}; stderr: {})",
                stderr.trim()
            );
            None
        }
    }
}

/// Ensure the container system service is running, starting it if necessary.
///
/// Returns `true` if the service is running after this call.
async fn ensure_system_running(container_cli: &str, status: &SystemStatus) -> bool {
    if status.status == "running" {
        println!("  status:  running \u{2713}");
        return true;
    }

    println!(
        "  status:  {} \u{2014} starting (with kernel install)...",
        status.status
    );
    match Command::new(container_cli)
        .args(["system", "start", "--enable-kernel-install"])
        .status()
        .await
    {
        Ok(s) if s.success() => {
            println!("  status:  started \u{2713}");
            true
        }
        Ok(s) => {
            println!(
                "  status:  `{container_cli} system start` failed (exit code: {:?})",
                s.code()
            );
            false
        }
        Err(e) => {
            println!("  status:  failed to start: {e}");
            false
        }
    }
}

// ── Kernel check ──────────────────────────────────────────────────────────────

/// Ensure a default kernel is installed under `<appRoot>/kernels/`.
///
/// `container system kernel set` stores kernels in the app-root filesystem
/// tree, which is not reflected in `container system property list`, so the
/// directory is checked directly using the `appRoot` field from the status JSON.
async fn ensure_kernel(container_cli: &str, status: &SystemStatus) {
    let kernel_present = status
        .app_root
        .as_deref()
        .filter(|r| !r.is_empty())
        .is_some_and(|root| {
            let kernels_dir = Path::new(root).join("kernels");
            std::fs::read_dir(kernels_dir).is_ok_and(|mut d| d.next().is_some())
        });

    if kernel_present {
        println!("  kernel:  configured \u{2713}");
        return;
    }

    println!("  kernel:  not configured \u{2014} installing recommended kernel...");
    match Command::new(container_cli)
        .args(["system", "kernel", "set", "--recommended"])
        .status()
        .await
    {
        Ok(s) if s.success() => println!("  kernel:  installed \u{2713}"),
        Ok(s) => {
            println!(
                "  kernel:  `{container_cli} system kernel set --recommended` failed \
                 (exit code: {:?})",
                s.code()
            );
            println!("           Run it manually for details.");
        }
        Err(e) => println!("  kernel:  failed to run: {e}"),
    }
}

// ── Rosetta check ─────────────────────────────────────────────────────────────

/// If Rosetta is not installed, disable `build.rosetta` and reset the
/// `BuildKit` builder so cross-arch builds don't fail at bootstrap.
async fn ensure_rosetta_disabled(container_cli: &str) {
    let rosetta_installed = Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());

    if rosetta_installed {
        return;
    }

    let prop_ok = Command::new(container_cli)
        .args(["system", "property", "set", "build.rosetta", "false"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());

    if prop_ok {
        println!("  build:   Rosetta not installed \u{2014} set build.rosetta=false \u{2713}");
    } else {
        println!("  build:   Rosetta not installed but could not set property");
        println!("           Run: {container_cli} system property set build.rosetta false");
    }

    // The BuildKit builder caches its launch config; reset it so the next
    // build starts fresh with the updated property.
    let stopped = Command::new(container_cli)
        .args(["builder", "stop"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    let deleted = Command::new(container_cli)
        .args(["builder", "delete", "--force"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());

    if stopped || deleted {
        println!("  build:   builder reset \u{2713}");
    }
}
