use std::{path::PathBuf, process::Stdio, sync::Arc};

use tokio::process::Command;
use tracing::debug;

use crate::{
    config::{EffectiveConfig, sanitize_package_name},
    error::NpxcError,
    paths::{SessionState, validate_path},
};

use super::{network::ManagedNetwork, proc::ContainerProcess};

// ── Launch plan types ─────────────────────────────────────────────────────────

/// Whether a mount is read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    Ro,
    Rw,
}

impl MountMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MountMode::Ro => "ro",
            MountMode::Rw => "rw",
        }
    }

    /// Parse a config-file mode string: `"rw"` → [`Rw`][MountMode::Rw],
    /// anything else (including `"ro"`) → [`Ro`][MountMode::Ro].
    #[must_use]
    pub fn from_config_str(s: &str) -> Self {
        if s == "rw" { Self::Rw } else { Self::Ro }
    }
}

/// A single filesystem bind-mount to add to the container.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Absolute host-side path.
    pub host: PathBuf,
    /// Absolute container-side path.
    pub container: String,
    pub mode: MountMode,
}

/// All caller-controlled variable parts of a `container run` invocation.
///
/// `Session::start` builds the fixed base flags and the workspace mount
/// internally, then appends everything in this struct.
///
/// Assemble with `LaunchPlan::default()` and fill fields as needed, or
/// collect them via `run_package`.
#[derive(Debug, Default)]
pub struct LaunchPlan {
    /// Extra bind-mounts beyond the session workspace.
    ///
    /// Each entry becomes a `-v host:container:mode` argument.  Paths must
    /// already be canonical absolute host paths.
    pub mounts: Vec<Mount>,

    /// Literal environment variables injected into the container.
    ///
    /// Each `(K, V)` pair becomes a `-e K=V` argument.
    pub env_literal: Vec<(String, String)>,

    /// Environment variable *names* forwarded from npxc's own process
    /// environment.  Each name becomes a bare `-e K` argument; the container
    /// runtime inherits the value from npxc's environment at launch time.
    /// Secrets never touch the npxc config file when this mechanism is used.
    pub env_passthrough: Vec<String>,

    /// Arguments appended after the image tag.
    ///
    /// These are forwarded verbatim to the container's entrypoint.
    pub args: Vec<String>,
}

impl LaunchPlan {
    /// Assemble a `LaunchPlan` from the resolved package configuration.
    ///
    /// Steps, in order:
    /// 1. Populate `env_literal` and `env_passthrough` from `effective`.
    /// 2. Store `args` verbatim.
    /// 3. Validate and add each config-declared directory mount.
    ///    Relative `host` paths are resolved against `cwd`; every path is
    ///    checked to lie within the CWD scope via [`validate_path`].
    /// 4. When `effective.storage.persist` is set, create the per-package
    ///    host directory under the platform data dir and add a read-write
    ///    mount at `/data`.
    /// 5. **`--no-isolate`** — when `no_isolate` is `true`, append a
    ///    read-only bind-mount of the canonicalized CWD at its own absolute
    ///    path so host paths resolve identically inside the guest.
    ///
    /// # Errors
    ///
    /// Returns [`NpxcError::PathOutOfScope`] or [`NpxcError::PathNotFound`]
    /// if a config-declared mount path fails CWD validation,
    /// [`NpxcError::Config`] if the platform data directory cannot be
    /// determined, or [`NpxcError::Io`] if the persistent storage directory
    /// cannot be created.
    pub fn build(
        pkg_name: &str,
        effective: &EffectiveConfig,
        cwd: &std::path::Path,
        args: Vec<String>,
        no_isolate: bool,
    ) -> Result<Self, NpxcError> {
        let mut plan = Self {
            args,
            env_literal: effective
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            env_passthrough: effective.env_passthrough.clone(),
            mounts: Vec::new(),
        };

        // Config-declared directory mounts, validated within CWD scope.
        for mc in &effective.mounts {
            let host_path = std::path::Path::new(&mc.host);
            let abs_host = if host_path.is_absolute() {
                host_path.to_path_buf()
            } else {
                cwd.join(host_path)
            };
            let canonical = validate_path(abs_host.to_str().unwrap_or(&mc.host), cwd)?;
            plan.mounts.push(Mount {
                host: canonical,
                container: mc.container.clone(),
                mode: MountMode::from_config_str(&mc.mode),
            });
        }

        // Persistent storage: per-package host dir mounted rw at /data.
        if effective.storage.as_ref().is_some_and(|s| s.persist) {
            let sanitized = sanitize_package_name(pkg_name);
            let data_dir = directories::ProjectDirs::from("", "", "npxc")
                .map(|dirs| dirs.data_dir().join("packages").join(&sanitized))
                .ok_or_else(|| {
                    NpxcError::Config("cannot determine platform data directory".into())
                })?;
            std::fs::create_dir_all(&data_dir)?;
            plan.mounts.push(Mount {
                host: data_dir,
                container: "/data".to_string(),
                mode: MountMode::Rw,
            });
        }

        // --no-isolate: expose the host CWD read-only at its canonical path.
        if no_isolate {
            let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
            plan.mounts.push(Mount {
                container: canonical.to_string_lossy().into_owned(),
                host: canonical,
                mode: MountMode::Ro,
            });
        }

        Ok(plan)
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

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
    /// The per-session container network npxc created (if any). Deleted on
    /// teardown, after the container has stopped.
    network: Option<ManagedNetwork>,
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
    /// `image_tag` with the constraints from `config` and the caller-supplied
    /// [`LaunchPlan`].
    ///
    /// # Fixed flags (always emitted)
    ///
    /// `--rm -i --progress none --name … --network … --read-only --tmpfs /tmp
    /// --cap-drop ALL -m <mem> -c <cpus>`
    ///
    /// # Variable parts (from `plan`)
    ///
    /// One `-v host:container:mode` per `plan.mounts` (after the workspace
    /// mount), one `-e K=V` per `plan.env_literal`, one `-e K` per
    /// `plan.env_passthrough`, then `image_tag`, then `plan.args`.
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
        network_arg: &str,
        plan: &LaunchPlan,
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
            network_arg,
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--cap-drop",
            "ALL",
            "-m",
            &config.memory,
            "-c",
            &config.cpus,
        ]);

        // Session workspace mount — always first.
        let workspace_spec = format!("{}:/workspace:{}", session_dir.display(), config.mount_mode);
        cmd.args(["-v", &workspace_spec]);

        // Extra mounts from the launch plan (--no-isolate CWD, storage, declared mounts).
        for mount in &plan.mounts {
            let spec = format!(
                "{}:{}:{}",
                mount.host.display(),
                mount.container,
                mount.mode.as_str(),
            );
            cmd.args(["-v", &spec]);
        }

        // Literal env vars (-e K=V).
        for (k, v) in &plan.env_literal {
            cmd.args(["-e", &format!("{k}={v}")]);
        }

        // Passthrough env var names (-e K — container inherits value from npxc env).
        for k in &plan.env_passthrough {
            cmd.args(["-e", k]);
        }

        // Image tag comes after all flags.
        cmd.arg(image_tag);

        // Args forwarded to the container's entrypoint.
        for arg in &plan.args {
            cmd.arg(arg);
        }

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
            network: None,
            cleaned: false,
        })
    }

    /// Attach a per-session [`ManagedNetwork`] for cleanup on teardown.
    ///
    /// The caller provisions the network before [`start`] and hands it over
    /// once the session exists, so the session owns deletion of the network it
    /// runs on.
    ///
    /// [`start`]: Session::start
    pub fn attach_network(&mut self, network: Option<ManagedNetwork>) {
        self.network = network;
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
        // Delete the network only after the container has stopped.
        if let Some(net) = self.network.take() {
            if let Err(e) = net.delete().await {
                tracing::warn!(
                    network = %net.name(),
                    error = %e,
                    "failed to delete per-session network",
                );
            }
        }
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
        if let Some(net) = &self.network {
            net.delete_blocking();
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::TempDir;

    use crate::{
        config::{
            EffectiveConfig, NetworkPolicy,
            package::{MountConfig, StorageConfig},
        },
        error::NpxcError,
    };

    use super::{LaunchPlan, MountMode};

    // Build a minimal `EffectiveConfig` with every field at its zero value.
    // Tests mutate only the fields they care about.
    fn base_config() -> EffectiveConfig {
        EffectiveConfig {
            node_image: String::new(),
            container_cli: String::new(),
            network: NetworkPolicy::None,
            memory: String::new(),
            cpus: String::new(),
            mount_mode: String::new(),
            log_level: String::new(),
            strategies: vec![],
            heuristic_absolute_prefix: false,
            heuristic_home_prefix: false,
            heuristic_uri_prefix: vec![],
            version: None,
            path_arguments: HashMap::new(),
            non_path_arguments: HashMap::new(),
            env: HashMap::new(),
            env_passthrough: vec![],
            storage: None,
            mounts: vec![],
        }
    }

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    // ── env ──────────────────────────────────────────────────────────────

    #[test]
    fn env_literal_populated_from_config() {
        let cwd = tmp();
        let mut config = base_config();
        config.env.insert("FOO".into(), "bar".into());
        config.env.insert("BAZ".into(), "qux".into());

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();

        let mut got: Vec<_> = plan.env_literal;
        got.sort();
        assert_eq!(
            got,
            [("BAZ".into(), "qux".into()), ("FOO".into(), "bar".into())]
        );
    }

    #[test]
    fn env_passthrough_populated_from_config() {
        let cwd = tmp();
        let mut config = base_config();
        config.env_passthrough = vec!["SECRET".into(), "TOKEN".into()];

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();

        assert_eq!(plan.env_passthrough, ["SECRET", "TOKEN"]);
    }

    // ── args ───────────────────────────────────────────────────────────

    #[test]
    fn args_forwarded_verbatim() {
        let cwd = tmp();
        let args = vec!["--port".into(), "3000".into()];

        let plan =
            LaunchPlan::build("pkg", &base_config(), cwd.path(), args.clone(), false).unwrap();

        assert_eq!(plan.args, args);
    }

    // ── Directory mounts ────────────────────────────────────────────────

    #[test]
    fn config_mount_absolute_ro_validated_and_added() {
        let cwd = tmp();
        let subdir = cwd.path().join("data");
        std::fs::create_dir(&subdir).unwrap();

        let mut config = base_config();
        config.mounts = vec![MountConfig {
            host: subdir.to_str().unwrap().into(),
            container: "/data".into(),
            mode: "ro".into(),
        }];

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();

        let canonical_cwd = cwd.path().canonicalize().unwrap();
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(plan.mounts[0].container, "/data");
        assert_eq!(plan.mounts[0].mode, MountMode::Ro);
        assert!(plan.mounts[0].host.starts_with(&canonical_cwd));
    }

    #[test]
    fn config_mount_relative_path_resolved_against_cwd() {
        let cwd = tmp();
        std::fs::create_dir(cwd.path().join("sub")).unwrap();

        let mut config = base_config();
        config.mounts = vec![MountConfig {
            host: "sub".into(),
            container: "/sub".into(),
            mode: "ro".into(),
        }];

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();

        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].host,
            cwd.path().join("sub").canonicalize().unwrap()
        );
    }

    #[test]
    fn config_mount_rw_mode_parsed_correctly() {
        let cwd = tmp();
        std::fs::create_dir(cwd.path().join("rw")).unwrap();

        let mut config = base_config();
        config.mounts = vec![MountConfig {
            host: cwd.path().join("rw").to_str().unwrap().into(),
            container: "/rw".into(),
            mode: "rw".into(),
        }];

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();

        assert_eq!(plan.mounts[0].mode, MountMode::Rw);
    }

    #[test]
    fn config_mount_outside_cwd_is_rejected() {
        let cwd = tmp();
        let outside = tmp(); // separate temp dir, not inside cwd

        let mut config = base_config();
        config.mounts = vec![MountConfig {
            host: outside.path().to_str().unwrap().into(),
            container: "/outside".into(),
            mode: "ro".into(),
        }];

        let result = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false);
        assert!(matches!(result, Err(NpxcError::PathOutOfScope { .. })));
    }

    #[test]
    fn config_mount_nonexistent_path_is_rejected() {
        let cwd = tmp();

        let mut config = base_config();
        config.mounts = vec![MountConfig {
            host: cwd.path().join("no-such-dir").to_str().unwrap().into(),
            container: "/missing".into(),
            mode: "ro".into(),
        }];

        let result = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false);
        assert!(matches!(result, Err(NpxcError::PathNotFound(_))));
    }

    // ── --no-isolate ─────────────────────────────────────────────────────

    #[test]
    fn no_isolate_appends_cwd_ro_mount() {
        let cwd = tmp();
        let canonical_cwd = cwd.path().canonicalize().unwrap();

        let plan = LaunchPlan::build("pkg", &base_config(), cwd.path(), vec![], true).unwrap();

        assert_eq!(plan.mounts.len(), 1);
        let m = &plan.mounts[0];
        assert_eq!(m.mode, MountMode::Ro);
        assert_eq!(m.host, canonical_cwd);
        // Container path mirrors the canonical host path (passthrough semantics).
        assert_eq!(m.container, canonical_cwd.to_string_lossy());
    }

    #[test]
    fn no_isolate_false_adds_no_cwd_mount() {
        let cwd = tmp();
        let plan = LaunchPlan::build("pkg", &base_config(), cwd.path(), vec![], false).unwrap();
        assert!(plan.mounts.is_empty());
    }

    // ── Persistent storage ──────────────────────────────────────────────

    #[test]
    fn persist_storage_mounts_data_dir_rw() {
        let cwd = tmp();
        let mut config = base_config();
        config.storage = Some(StorageConfig {
            persist: true,
            writable: vec![],
        });

        let plan = LaunchPlan::build("@scope/my-pkg", &config, cwd.path(), vec![], false).unwrap();

        assert_eq!(plan.mounts.len(), 1);
        let m = &plan.mounts[0];
        assert_eq!(m.container, "/data");
        assert_eq!(m.mode, MountMode::Rw);
        // The host dir must have been created.
        assert!(m.host.is_dir());
        // Sanitized package name appears in the path.
        assert!(m.host.to_string_lossy().contains("scope-my-pkg"));
    }

    #[test]
    fn no_persist_adds_no_mount() {
        let cwd = tmp();
        let mut config = base_config();
        config.storage = Some(StorageConfig {
            persist: false,
            writable: vec![],
        });

        let plan = LaunchPlan::build("pkg", &config, cwd.path(), vec![], false).unwrap();
        assert!(plan.mounts.is_empty());
    }
}
