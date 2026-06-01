use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;

use crate::{
    config::{EffectiveConfig, sanitize_package_name},
    dockerfile,
    error::NpxcError,
};

// ── Tag helpers ───────────────────────────────────────────────────────────────

/// Canonical image tag for a package at a specific version.
///
/// Format: `npxc/<sanitized-package>:<version>`
///
/// # Examples
/// ```
/// # use npxc::runtime::image_tag;
/// assert_eq!(image_tag("@scope/my-tool", "1.2.3"), "npxc/scope-my-tool:1.2.3");
/// assert_eq!(image_tag("express", "latest"),        "npxc/express:latest");
/// ```
#[must_use]
pub fn image_tag(pkg_name: &str, version: &str) -> String {
    format!("npxc/{}:{}", sanitize_package_name(pkg_name), version)
}

/// `boringtun-cli` version compiled into the `WireGuard` base image.
const BORINGTUN_VERSION: &str = "0.7.1";

/// Tag of the prebuilt `WireGuard` base image. Versioned by [`BORINGTUN_VERSION`]
/// so a version bump produces a new tag (and a one-time recompile) rather than
/// silently reusing a stale binary.
fn wg_base_tag() -> String {
    format!("npxc/wg-base:{BORINGTUN_VERSION}")
}

// ── Inspect / existence check ─────────────────────────────────────────────────

/// Return `true` if `tag` exists in the local image store, `false` if not.
///
/// Runs `<container_cli> image inspect <tag>`. Exit code 0 means the image
/// exists; any non-zero exit is treated as "not found". An error spawning the
/// command is returned as [`NpxcError::RuntimeNotAvailable`].
///
/// # Errors
///
/// Returns [`NpxcError::RuntimeNotAvailable`] if the container CLI cannot be
/// spawned.
pub async fn image_exists(container_cli: &str, tag: &str) -> Result<bool, NpxcError> {
    let mut cmd = Command::new(container_cli);
    cmd.args(["image", "inspect", tag]);
    // Suppress noisy output from the inspect command.
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    debug!(cmd = ?cmd, "running container command");

    let status = cmd.status().await.map_err(|e| {
        NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
    })?;

    Ok(status.success())
}

// ── Build ─────────────────────────────────────────────────────────────────────

/// Build a container image for `pkg_name` at `version`.
///
/// Steps:
/// 1. Render the Dockerfile template and write it to a temporary directory.
/// 2. Run `<container_cli> build` with `--build-arg` flags, tagging the image
///    as `npxc/<sanitized-name>:<version>`.
/// 3. Stream build output to stderr so the user can see progress.
///
/// Returns [`NpxcError::BuildFailed`] if the build command exits non-zero.
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the Dockerfile cannot be written to the build
/// context, [`NpxcError::RuntimeNotAvailable`] if the container CLI cannot be
/// spawned, or [`NpxcError::BuildFailed`] if the build exits non-zero.
pub async fn build_image(
    pkg_name: &str,
    version: &str,
    config: &EffectiveConfig,
    force_rebuild: bool,
) -> Result<(), NpxcError> {
    let tag = image_tag(pkg_name, version);
    let package_spec = format!("{pkg_name}@{version}");

    // Compile the userspace-WireGuard binary once into a reusable base image;
    // the package build below copies it instead of recompiling.
    ensure_wg_base_image(&config.container_cli).await?;

    // Write the Dockerfile into a temporary build context directory.
    // The TempDir is held for the lifetime of this function; it is deleted on
    // drop (after the build command exits).
    let tmp = tempfile::Builder::new()
        .prefix("npxc-build-")
        .tempdir()
        .map_err(NpxcError::Io)?;

    // PACKAGE_SPEC / NODE_IMAGE are passed via --build-arg below. The WireGuard
    // base image tag is substituted into the template here because `COPY --from`
    // can't reference a build ARG in this BuildKit frontend.
    let dockerfile =
        dockerfile::DOCKERFILE_TEMPLATE.replace("__NPXC_WG_BASE_IMAGE__", &wg_base_tag());
    let dockerfile_path = tmp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, dockerfile)?;

    let mut cmd = Command::new(&config.container_cli);
    cmd.arg("build");
    // Always target linux/arm64 (npxc is macOS Apple Silicon only).
    // This prevents the BuildKit bootstrap from requiring Rosetta, which the
    // container system enables by default for cross-arch support but which may
    // not be installed.
    cmd.args(["--platform", "linux/arm64"]);
    cmd.args(["--build-arg", &format!("PACKAGE_SPEC={package_spec}")]);
    cmd.args(["--build-arg", &format!("NODE_IMAGE={}", config.node_image)]);
    cmd.args(["-t", &tag]);
    cmd.args(["-f", &dockerfile_path.to_string_lossy()]);
    if force_rebuild {
        cmd.arg("--no-cache");
    }
    // The build context is the temp directory.
    cmd.arg(tmp.path());

    // Inherit stderr so the user sees build progress.
    cmd.stderr(Stdio::inherit());

    debug!(cmd = ?cmd, "running container command");

    let status = cmd.status().await.map_err(|e| {
        NpxcError::RuntimeNotAvailable(format!("failed to spawn '{}': {e}", config.container_cli))
    })?;

    // Tear the BuildKit builder down so no build state lingers between npxc
    // invocations. The expensive WireGuard compile is cached as a persisted
    // base image (see `ensure_wg_base_image`), not in the builder, so deleting
    // the builder costs nothing on the next build.
    stop_builder(&config.container_cli).await;

    if status.success() {
        Ok(())
    } else {
        Err(NpxcError::BuildFailed {
            code: status.code(),
        })
    }
}

/// Ensure the prebuilt `WireGuard` base image ([`wg_base_tag`]) exists, building
/// it once if it doesn't.
///
/// This is where the expensive `cargo install boringtun-cli` runs — exactly
/// once per `boringtun-cli` version. The result is a tiny image in the local
/// store, so it survives builder teardown and every later package build just
/// `COPY --from`s the finished binary.
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the Dockerfile can't be written,
/// [`NpxcError::RuntimeNotAvailable`] if the CLI can't be spawned, or
/// [`NpxcError::BuildFailed`] if the base build exits non-zero.
async fn ensure_wg_base_image(container_cli: &str) -> Result<(), NpxcError> {
    let tag = wg_base_tag();
    if image_exists(container_cli, &tag).await? {
        tracing::debug!(%tag, "WireGuard base image present, skipping build");
        return Ok(());
    }

    tracing::info!(
        %tag,
        "building WireGuard base image (one-time; compiles boringtun-cli)"
    );

    let tmp = tempfile::Builder::new()
        .prefix("npxc-wgbase-")
        .tempdir()
        .map_err(NpxcError::Io)?;
    let dockerfile_path = tmp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, dockerfile::WG_BASE_DOCKERFILE)?;

    let mut cmd = Command::new(container_cli);
    cmd.arg("build");
    cmd.args(["--platform", "linux/arm64"]);
    cmd.args([
        "--build-arg",
        &format!("BORINGTUN_VERSION={BORINGTUN_VERSION}"),
    ]);
    cmd.args(["-t", &tag]);
    cmd.args(["-f", &dockerfile_path.to_string_lossy()]);
    cmd.arg(tmp.path());
    cmd.stderr(Stdio::inherit());

    debug!(cmd = ?cmd, "running container command");

    let status = cmd.status().await.map_err(|e| {
        NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
    })?;

    stop_builder(container_cli).await;

    if status.success() {
        Ok(())
    } else {
        Err(NpxcError::BuildFailed {
            code: status.code(),
        })
    }
}

/// Stop and delete the `BuildKit` builder container (best-effort; errors are
/// logged at debug level and do not propagate).
///
/// Deleting keeps a clean slate between builds: no builder VM or build cache
/// lingers. This is cheap because the only expensive step — compiling
/// `boringtun-cli` — is cached as a standalone base image in the image store
/// (see [`ensure_wg_base_image`]), independent of the builder's lifecycle.
async fn stop_builder(container_cli: &str) {
    for args in [
        ["builder", "stop"].as_slice(),
        &["builder", "delete", "--force"],
    ] {
        let result = Command::new(container_cli)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if let Err(e) = result {
            debug!("builder cleanup ({} {}): {e}", args[0], args[1]);
        }
    }
}

// ── Ensure ────────────────────────────────────────────────────────────────────

/// Ensure the image for `pkg_name@version` is present, building it if needed.
///
/// If `force_rebuild` is `true` the image is always rebuilt (with
/// `--no-cache`), even if it already exists.
///
/// Returns the image tag on success.
///
/// # Errors
///
/// Propagates any error from [`image_exists`] or [`build_image`].
pub async fn ensure_image(
    pkg_name: &str,
    version: &str,
    config: &EffectiveConfig,
    force_rebuild: bool,
) -> Result<String, NpxcError> {
    let tag = image_tag(pkg_name, version);

    if !force_rebuild && image_exists(&config.container_cli, &tag).await? {
        tracing::debug!(%tag, "image already exists, skipping build");
        return Ok(tag);
    }

    build_image(pkg_name, version, config, force_rebuild).await?;
    Ok(tag)
}

// ── Listing ───────────────────────────────────────────────────────────────────

/// Minimal image record from `container image list --format json`.
///
/// Only `reference` is required; the full OCI descriptor is ignored.
#[derive(serde::Deserialize)]
struct ContainerImage {
    reference: String,
}

/// List npxc-managed **package** images in the local image store.
///
/// Runs `<container_cli> image list --format json` and returns a
/// `Vec<(repository, tag)>` for every image whose reference starts with
/// `"npxc/"`, excluding the internal [`wg_base_tag`] base image — it's a build
/// dependency, not a package, so it stays out of `npxc list` and survives
/// `npxc clean --all` (avoiding a needless one-time `boringtun-cli` recompile).
/// Remove it manually (`container image rm npxc/wg-base:<ver>`) if desired.
///
/// # Errors
///
/// Returns [`NpxcError::RuntimeNotAvailable`] if the container CLI cannot be
/// spawned, [`NpxcError::Runtime`] if the command exits non-zero, or
/// [`NpxcError::Json`] if the output is not valid JSON.
pub async fn list_images(container_cli: &str) -> Result<Vec<(String, String)>, NpxcError> {
    let mut cmd = Command::new(container_cli);
    cmd.args(["image", "list", "--format", "json"]);

    debug!(cmd = ?cmd, "running container command");

    let output = cmd.output().await.map_err(|e| {
        NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NpxcError::Runtime(format!(
            "`{container_cli} image list` failed: {}",
            stderr.trim()
        )));
    }

    let all: Vec<ContainerImage> = serde_json::from_slice(&output.stdout)?;

    Ok(all
        .into_iter()
        .filter(|img| {
            img.reference.starts_with("npxc/") && !img.reference.starts_with("npxc/wg-base:")
        })
        .filter_map(|img| {
            // Split on the last `:` to separate repository from tag.
            // npxc images never contain a registry prefix, so the `npxc/`
            // filter above guarantees the colon is the tag separator.
            let (repo, tag) = img.reference.rsplit_once(':')?;
            Some((repo.to_string(), tag.to_string()))
        })
        .collect())
}

// ── Removal ───────────────────────────────────────────────────────────────────

/// Remove a container image by tag.
///
/// Returns [`NpxcError::Runtime`] if the command exits non-zero.
///
/// # Errors
///
/// Returns [`NpxcError::RuntimeNotAvailable`] if the container CLI cannot be
/// spawned, or [`NpxcError::Runtime`] if the removal command exits non-zero.
pub async fn remove_image(container_cli: &str, tag: &str) -> Result<(), NpxcError> {
    let mut cmd = Command::new(container_cli);
    cmd.args(["image", "rm", tag]);

    debug!(cmd = ?cmd, "running container command");

    let status = cmd.status().await.map_err(|e| {
        NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
    })?;

    if status.success() {
        Ok(())
    } else {
        Err(NpxcError::Runtime(format!(
            "failed to remove image '{tag}' (exit code: {:?})",
            status.code()
        )))
    }
}
