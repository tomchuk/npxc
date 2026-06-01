//! Per-session container network lifecycle.
//!
//! npxc can run a sandboxed container on a dedicated, host-only (`--internal`)
//! `container` network that it creates for the session and deletes on teardown.
//! A host-only network has no NAT route to the internet, so the container has
//! no direct egress — a later phase routes filtered egress through npxc itself.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

use crate::{config::NetworkPolicy, error::NpxcError};

/// A `container` network created and owned by npxc for a single session.
///
/// Created by [`provision`] when the resolved [`NetworkPolicy`] requires an
/// isolated network; deleted by [`delete`] (async, preferred) or
/// [`delete_blocking`] (sync, best-effort) once the attached container has
/// stopped.
///
/// [`provision`]: ManagedNetwork::provision
/// [`delete`]: ManagedNetwork::delete
/// [`delete_blocking`]: ManagedNetwork::delete_blocking
pub struct ManagedNetwork {
    name: String,
    container_cli: String,
    /// IPv4 subnet assigned to the network (e.g. `192.168.66.0/24`).
    pub subnet: String,
    /// IPv4 gateway — the host's address on the network (e.g. `192.168.66.1`).
    pub gateway: String,
}

/// Minimal projection of one `container network inspect` array element.
#[derive(serde::Deserialize)]
struct NetworkInspect {
    status: NetworkInspectStatus,
}

#[derive(serde::Deserialize)]
struct NetworkInspectStatus {
    #[serde(rename = "ipv4Subnet")]
    ipv4_subnet: String,
    #[serde(rename = "ipv4Gateway")]
    ipv4_gateway: String,
}

impl ManagedNetwork {
    /// The container network's name (used as the `--network` argument).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolve a [`NetworkPolicy`] into the `--network` argument value plus an
    /// optional owned network.
    ///
    /// - [`NetworkPolicy::None`] → `("none", None)`.
    /// - [`NetworkPolicy::Named`] → `(name, None)` (pass-through).
    /// - [`NetworkPolicy::Allowlist`] → create a per-session `--internal`
    ///   network and return `(its_name, Some(network))`.
    ///
    /// # Errors
    ///
    /// Returns an error if creating or inspecting the isolated network fails.
    pub async fn provision(
        policy: &NetworkPolicy,
        container_cli: &str,
    ) -> Result<(String, Option<Self>), NpxcError> {
        match policy {
            NetworkPolicy::None => Ok(("none".to_string(), None)),
            NetworkPolicy::Named(name) => Ok((name.clone(), None)),
            NetworkPolicy::Allowlist { .. } => {
                let net = Self::create_internal(container_cli).await?;
                Ok((net.name.clone(), Some(net)))
            }
        }
    }

    /// Create a fresh host-only network with a unique name and read back its
    /// assigned subnet/gateway via `network inspect`.
    async fn create_internal(container_cli: &str) -> Result<Self, NpxcError> {
        let name = format!("npxc-{}", uuid::Uuid::new_v4().simple());

        let mut cmd = Command::new(container_cli);
        cmd.args(["network", "create", "--internal", &name]);
        debug!(cmd = ?cmd, "creating per-session network");

        let output = cmd.output().await.map_err(|e| {
            NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NpxcError::Runtime(format!(
                "`{container_cli} network create --internal {name}` failed: {}",
                stderr.trim()
            )));
        }

        // Read subnet/gateway; clean up the network if inspect fails so we
        // don't leak a half-provisioned network.
        match Self::inspect(container_cli, &name).await {
            Ok((subnet, gateway)) => Ok(Self {
                name,
                container_cli: container_cli.to_string(),
                subnet,
                gateway,
            }),
            Err(e) => {
                let stale = Self {
                    name,
                    container_cli: container_cli.to_string(),
                    subnet: String::new(),
                    gateway: String::new(),
                };
                stale.delete_blocking();
                Err(e)
            }
        }
    }

    /// Run `network inspect <name>` and extract the IPv4 subnet + gateway.
    async fn inspect(container_cli: &str, name: &str) -> Result<(String, String), NpxcError> {
        let mut cmd = Command::new(container_cli);
        cmd.args(["network", "inspect", name]);
        debug!(cmd = ?cmd, "inspecting network");

        let output = cmd.output().await.map_err(|e| {
            NpxcError::RuntimeNotAvailable(format!("failed to spawn '{container_cli}': {e}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NpxcError::Runtime(format!(
                "`{container_cli} network inspect {name}` failed: {}",
                stderr.trim()
            )));
        }
        parse_inspect(&output.stdout)
    }

    /// Delete the network (async). Call only after the attached container has
    /// stopped — `container` refuses to delete an in-use network.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete command cannot be spawned or exits
    /// non-zero.
    pub async fn delete(&self) -> Result<(), NpxcError> {
        let status = Command::new(&self.container_cli)
            .args(["network", "delete", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| {
                NpxcError::RuntimeNotAvailable(format!(
                    "failed to spawn '{}': {e}",
                    self.container_cli
                ))
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(NpxcError::Runtime(format!(
                "failed to delete network '{}' (exit code: {:?})",
                self.name,
                status.code()
            )))
        }
    }

    /// Delete the network, retrying a few times before giving up.
    ///
    /// `container` refuses to delete a network that's still in use; right after
    /// the attached container is force-removed there can be a brief window where
    /// the runtime still reports it busy. A handful of backed-off retries closes
    /// that race. Warns (does not error) if it ultimately fails.
    pub async fn delete_with_retry(&self) {
        const ATTEMPTS: u32 = 5;
        for attempt in 1..=ATTEMPTS {
            match self.delete().await {
                Ok(()) => return,
                Err(e) => {
                    if attempt == ATTEMPTS {
                        tracing::warn!(
                            network = %self.name,
                            error = %e,
                            "failed to delete per-session network after {ATTEMPTS} attempts",
                        );
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(150 * u64::from(attempt))).await;
                }
            }
        }
    }

    /// Best-effort synchronous deletion for the `Drop` path.
    pub fn delete_blocking(&self) {
        let _ = std::process::Command::new(&self.container_cli)
            .args(["network", "delete", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Parse the JSON array emitted by `container network inspect` and return the
/// first element's `(ipv4Subnet, ipv4Gateway)`.
fn parse_inspect(stdout: &[u8]) -> Result<(String, String), NpxcError> {
    let items: Vec<NetworkInspect> = serde_json::from_slice(stdout)?;
    let first = items
        .into_iter()
        .next()
        .ok_or_else(|| NpxcError::Runtime("empty `network inspect` output".to_string()))?;
    Ok((first.status.ipv4_subnet, first.status.ipv4_gateway))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inspect_extracts_subnet_and_gateway() {
        // Shape of `container network inspect <name>` — a JSON array of
        // NetworkResource { id, configuration, status }.
        let json = br#"[
          {
            "id": "npxc-abc",
            "configuration": { "id": "npxc-abc", "mode": "hostOnly" },
            "status": {
              "ipv4Subnet": "192.168.66.0/24",
              "ipv4Gateway": "192.168.66.1",
              "ipv6Subnet": "fd00::/64"
            }
          }
        ]"#;
        let (subnet, gateway) = parse_inspect(json).unwrap();
        assert_eq!(subnet, "192.168.66.0/24");
        assert_eq!(gateway, "192.168.66.1");
    }

    #[test]
    fn parse_inspect_empty_array_errors() {
        let err = parse_inspect(b"[]").unwrap_err();
        assert!(matches!(err, NpxcError::Runtime(_)));
    }

    #[test]
    fn parse_inspect_invalid_json_errors() {
        let err = parse_inspect(b"not json").unwrap_err();
        assert!(matches!(err, NpxcError::Json(_)));
    }

    #[tokio::test]
    async fn provision_none_yields_none_arg_and_no_network() {
        let (arg, net) = ManagedNetwork::provision(&NetworkPolicy::None, "container")
            .await
            .unwrap();
        assert_eq!(arg, "none");
        assert!(net.is_none());
    }

    #[tokio::test]
    async fn provision_named_passes_through_without_creating() {
        let (arg, net) =
            ManagedNetwork::provision(&NetworkPolicy::Named("default".to_string()), "container")
                .await
                .unwrap();
        assert_eq!(arg, "default");
        assert!(net.is_none());
    }
}
