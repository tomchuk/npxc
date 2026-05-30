//! The Dockerfile template used to build per-package container images.
//!
//! `PACKAGE_SPEC` and `NODE_IMAGE` are substituted by the container build
//! engine via `--build-arg` flags (see [`crate::runtime::build`]), so the
//! template is written to the build context verbatim.

/// The raw Dockerfile template string, embedded at compile time.
pub const DOCKERFILE_TEMPLATE: &str = include_str!("../templates/Dockerfile.template");
