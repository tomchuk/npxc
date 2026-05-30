use std::{path::PathBuf, process::Stdio, sync::Arc};

use tokio::process::Command;
use tracing::debug;

use crate::{
    config::{EffectiveConfig, sanitize_package_name},
    error::NpxcError,
    paths::SessionState,
};

use super::proc::ContainerProcess;

/// An active container session for a single `npxc` invocation.
///
/// Holds the running container process (until [`take_container`] is called),
/// the temporary workspace directory mounted into the container, and the
/// shared [`SessionState`] used by the RPC pipeline.
///
/// Cleanup is performed either by [`teardown`] (async, graceful) or by the
/// [`Drop`] implementation (sync, best-effort).
///
/// [`take_container`]: Session::take_container
/// [`teardown`]: Session::teardown
pub struct Session {
    pub state: Arc<SessionState>,
    /// Host path of the temporary workspace directory mounted into the
    /// container as `/workspace`.
    pub session_dir: PathBuf,
    /// The running container process. `None` after [`take_container`] is
    /// called.
    ///
    /// [`take_container`]: Session::take_container
    container: Option<ContainerProcess>,
    /// Set once async [`teardown`] has cleaned up, so the synchronous [`Drop`]
    /// impl knows to skip its best-effort pass.
    ///
    /// [`teardown`]: Session::teardown
    cleaned: bool,
}

impl Session {
    /// Spawn a new container session.
    ///
    /// Creates a temporary workspace directory under `session_dir_parent`
    /// (defaults to [`std::env::temp_dir`]) and starts the container image
    /// `image_tag` with the constraints from `config`.
    ///
    /// The container is launched with:
    /// - `--rm -i` — remove on stop, keep stdin open
    /// - `--network <net>` — as configured
    /// - `--read-only --tmpfs /tmp` — immutable root with a writable `/tmp`
    /// - `--cap-drop ALL` — drop every Linux capability
    /// - `-m <mem> -c <cpus>` — resource limits
    /// - `-v <session_dir>:/workspace:<mount_mode>`
    ///
    /// If `extra_ro_mount` is `Some(dir)` (the `--no-isolate` escape hatch),
    /// `dir` is additionally mounted read-only at its own canonical absolute
    /// path, so the package can resolve in-scope host paths directly without
    /// per-file publication.
    ///
    /// # Errors
    ///
    /// Returns [`NpxcError::Io`] if the temporary workspace directory cannot be
    /// created, or [`NpxcError::RuntimeNotAvailable`] if the container process
    /// cannot be spawned.
    pub fn start(
        pkg_name: &str,
        image_tag: &str,
        config: &EffectiveConfig,
        extra_ro_mount: Option<&std::path::Path>,
        session_dir_parent: Option<&std::path::Path>,
    ) -> Result<Self, NpxcError> {
        let sanitized = sanitize_package_name(pkg_name);
        let parent =
            session_dir_parent.map_or_else(std::env::temp_dir, std::path::Path::to_path_buf);

        // Create the session directory and `keep()` it so it is NOT deleted
        // when the `TempDir` drops — npxc owns its lifecycle from here (cleaned
        // up by `teardown`/`Drop`).
        let session_dir = tempfile::Builder::new()
            .prefix(&format!("npxc-{sanitized}-"))
            .tempdir_in(&parent)
            .map_err(NpxcError::Io)?
            .keep();

        let mount_spec = format!("{}:/workspace:{}", session_dir.display(), config.mount_mode);

        // Use a deterministic, human-readable container name so it shows up
        // clearly in `container ls`. Include the PID to allow multiple
        // concurrent sessions of the same package.
        let container_name = format!("npxc-{sanitized}-{}", std::process::id());

        let mut cmd = Command::new(&config.container_cli);
        cmd.args([
            "run",
            "--rm",
            "-i",
            "--progress",
            "none",
            "--name",
            &container_name,
            "--network",
            &config.network,
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--cap-drop",
            "ALL",
            "-m",
            &config.memory,
            "-c",
            &config.cpus,
            "-v",
            &mount_spec,
        ]);

        // --no-isolate escape hatch: expose the host CWD at its real absolute
        // path, read-only, so host paths resolve identically inside the guest.
        if let Some(dir) = extra_ro_mount {
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            let spec = format!("{0}:{0}:ro", canonical.display());
            cmd.args(["-v", &spec]);
        }

        // The image tag must come after all flags.
        cmd.arg(image_tag);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // If the `Child` is dropped without an explicit kill (e.g. the pipeline
        // future is dropped on Ctrl-C), still terminate the container process.
        cmd.kill_on_drop(true);

        debug!(cmd = ?cmd, "running container command");

        let child = cmd.spawn().map_err(|e| {
            NpxcError::RuntimeNotAvailable(format!(
                "failed to spawn '{}': {e}",
                config.container_cli
            ))
        })?;

        let container = ContainerProcess::from_child(child);

        Ok(Session {
            state: Arc::new(SessionState::new()),
            session_dir,
            container: Some(container),
            cleaned: false,
        })
    }

    /// Transfer ownership of the container process to the caller.
    ///
    /// After this call `self.container` is `None`. The caller is responsible
    /// for shutting down the container process; the `Drop` impl on `Session`
    /// will still remove `session_dir` but will not attempt to kill an absent
    /// container.
    pub fn take_container(&mut self) -> Option<ContainerProcess> {
        self.container.take()
    }

    /// Graceful async shutdown: kill the container (if still present), wait
    /// for it to exit, then remove the session directory.
    ///
    /// Sets the `cleaned` flag so the synchronous [`Drop`] impl skips its
    /// redundant best-effort pass when `self` is dropped at the end of this
    /// method.
    pub async fn teardown(mut self) {
        if let Some(ref mut c) = self.container {
            c.kill_and_wait().await;
        }
        let _ = tokio::fs::remove_dir_all(&self.session_dir).await;
        self.cleaned = true;
    }
}

impl Drop for Session {
    /// Best-effort synchronous cleanup. Sends SIGKILL without waiting, then
    /// removes the session directory. This runs when the session is dropped
    /// without calling [`teardown`]; it is a no-op once `teardown` has already
    /// cleaned up.
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        if let Some(ref mut c) = self.container {
            c.kill_now();
        }
        let _ = std::fs::remove_dir_all(&self.session_dir);
    }
}
