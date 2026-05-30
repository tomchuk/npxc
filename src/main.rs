//! `npxc` binary entry-point.
//!
//! Module declarations live in `lib.rs`; this file references the library
//! crate as `npxc::` — the standard pattern for a mixed bin+lib Rust crate.

use clap::Parser;

use npxc::{
    cli::{Cli, Commands, GlobalOpts},
    config,
    error::NpxcError,
    rpc::pipeline,
    runtime::{Session, ensure_image, image_tag, list_images, remove_image},
};

// ── Entry-point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialise tracing from CLI flag or NPXC_LOG env var.
    let filter = cli.global.log_level.clone();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&filter)),
        )
        .init();

    if let Err(e) = run(cli).await {
        eprintln!("npxc: error: {e}");
        std::process::exit(e.exit_code());
    }
}

async fn run(cli: Cli) -> Result<(), NpxcError> {
    match cli.command {
        Commands::Run(args) => run_package(args, cli.global).await,
        Commands::Build { package_spec } => cmd_build(package_spec, cli.global, false).await,
        Commands::Rebuild { package_spec } => cmd_build(package_spec, cli.global, true).await,
        Commands::List => cmd_list(cli.global).await,
        Commands::Clean { package_spec, all } => cmd_clean(package_spec, all, cli.global).await,
        Commands::Inspect { package_spec } => cmd_inspect(&package_spec, &cli.global),
        Commands::Doctor => cmd_doctor(cli.global).await,
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Start a sandboxed MCP session for `args[0]` (the package spec).
///
/// `args[1..]` (with bare `--` separators stripped) are the package's own
/// arguments; they are captured for future use but not yet forwarded to the
/// container CLI.
async fn run_package(args: Vec<String>, global: GlobalOpts) -> Result<(), NpxcError> {
    let pkg_spec = args.first().cloned().unwrap_or_default();
    // Filter bare "--" separators; args are forwarded to the container image.
    let _pkg_args: Vec<&String> = args[1..].iter().filter(|a| *a != "--").collect();

    let config_path = global.config.as_ref();
    let (effective, pkg_name, version) = config::resolve_config(&pkg_spec, config_path)?;

    if global.no_isolate {
        eprintln!(
            "npxc: WARNING: --no-isolate disables per-file scoping and mounts the entire \
             CWD read-only into the container"
        );
    }

    let cwd = match &global.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };

    if global.dry_run {
        let tag = image_tag(&pkg_name, &version);
        println!("package:    {pkg_name}");
        println!("version:    {version}");
        println!("image_tag:  {tag}");
        println!("no_isolate: {}", global.no_isolate);
        return Ok(());
    }

    // Build the image if it does not already exist.
    let tag = ensure_image(&pkg_name, &version, &effective, false).await?;

    // Persist the resolved version (best-effort; log but don't fail).
    if let Err(e) = config::ensure_version_pinned(&pkg_name, &version, config_path) {
        tracing::error!("failed to pin version for {pkg_name}@{version}: {e}");
    }

    // In --no-isolate mode, mount the whole CWD read-only instead of
    // publishing individual files on demand.
    let extra_ro_mount = global.no_isolate.then_some(cwd.as_path());
    let mut session = Session::start(&pkg_name, &tag, &effective, extra_ro_mount, None)?;

    tracing::info!(
        package = %pkg_name,
        version = %version,
        tag = %tag,
        session_dir = %session.session_dir.display(),
        cwd = %cwd.display(),
        no_isolate = global.no_isolate,
        "starting npxc session",
    );

    let no_isolate = global.no_isolate;

    // Race the pipeline against Ctrl-C.
    tokio::select! {
        result = pipeline::run_pipeline(&mut session, &cwd, &effective, no_isolate) => {
            session.teardown().await;
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            session.teardown().await;
            std::process::exit(130);
        }
    }

    Ok(())
}

/// Build (or force-rebuild) the container image for a package.
async fn cmd_build(
    package_spec: String,
    global: GlobalOpts,
    force_rebuild: bool,
) -> Result<(), NpxcError> {
    let (effective, pkg_name, version) =
        config::resolve_config(&package_spec, global.config.as_ref())?;
    ensure_image(&pkg_name, &version, &effective, force_rebuild).await?;
    let sanitized = config::sanitize_package_name(&pkg_name);
    println!("Built image: npxc/{sanitized}:{version}");
    Ok(())
}

/// List all npxc-managed images in the local container store.
async fn cmd_list(global: GlobalOpts) -> Result<(), NpxcError> {
    let cfg = config::load_global_config(global.config.as_ref())?;
    let container_cli = cfg.defaults.container_cli;
    let images = list_images(&container_cli).await?;
    for (repo, tag) in images {
        println!("{repo}:{tag}");
    }
    Ok(())
}

/// Remove one or all npxc-managed images from the local container store.
async fn cmd_clean(
    package_spec: Option<String>,
    all: bool,
    global: GlobalOpts,
) -> Result<(), NpxcError> {
    let cfg = config::load_global_config(global.config.as_ref())?;
    let container_cli = cfg.defaults.container_cli;

    if all {
        let images = list_images(&container_cli).await?;
        for (repo, tag) in images {
            let full_tag = format!("{repo}:{tag}");
            remove_image(&container_cli, &full_tag).await?;
        }
    } else if let Some(spec) = package_spec {
        let (_effective, pkg_name, version) =
            config::resolve_config(&spec, global.config.as_ref())?;
        let tag = image_tag(&pkg_name, &version);
        remove_image(&container_cli, &tag).await?;
    } else {
        return Err(NpxcError::Config(
            "specify a package spec or pass --all to remove every cached image".into(),
        ));
    }

    Ok(())
}

/// Print the resolved configuration and image information for a package, then exit.
///
/// This command is synchronous (no container I/O).
fn cmd_inspect(package_spec: &str, global: &GlobalOpts) -> Result<(), NpxcError> {
    let (effective, pkg_name, version) =
        config::resolve_config(package_spec, global.config.as_ref())?;
    let tag = image_tag(&pkg_name, &version);

    println!("package:       {pkg_name}");
    println!("version:       {version}");
    println!("image_tag:     {tag}");
    println!("container_cli: {}", effective.container_cli);
    println!("node_image:    {}", effective.node_image);
    println!("network:       {}", effective.network);
    println!("memory:        {}", effective.memory);
    println!("cpus:          {}", effective.cpus);
    println!("mount_mode:    {}", effective.mount_mode);
    println!("strategies:    {:?}", effective.strategies);

    Ok(())
}

/// Check prerequisites: verify the container runtime is available, print its
/// version, and run `container system install` to ensure the VM kernel is
/// configured.
async fn cmd_doctor(global: GlobalOpts) -> Result<(), NpxcError> {
    let cfg = config::load_global_config(global.config.as_ref())?;
    let container_cli = cfg.defaults.container_cli;

    if !doctor_report_cli(&container_cli).await {
        return Ok(());
    }

    println!();
    println!("container system:");

    let status_text = doctor_system_status_text(&container_cli).await;
    if doctor_ensure_system_running(&container_cli, &status_text).await {
        doctor_ensure_kernel(&container_cli, &status_text).await;
    }

    doctor_ensure_rosetta_disabled(&container_cli).await;

    Ok(())
}

/// Verify the container CLI is on `PATH` and print its version. Returns `false`
/// (after printing install instructions) when the CLI is not found.
async fn doctor_report_cli(container_cli: &str) -> bool {
    let Ok(bin_path) = which::which(container_cli) else {
        println!("container CLI:  {container_cli} (NOT FOUND)");
        println!("  Install from: https://github.com/apple/container/releases");
        return false;
    };
    println!("container CLI:  {} (found)", bin_path.display());

    match tokio::process::Command::new(container_cli)
        .arg("--version")
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version_str = stdout.trim();
            if version_str.is_empty() {
                // Apple container prints version to stderr.
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("  version:      {}", stderr.trim());
            } else {
                println!("  version:      {version_str}");
            }
        }
        Err(e) => println!("  version:      (error running `{container_cli} --version`: {e})"),
    }
    true
}

/// Capture the stdout of `container system status` (empty string on error).
async fn doctor_system_status_text(container_cli: &str) -> String {
    let bytes = tokio::process::Command::new(container_cli)
        .args(["system", "status"])
        .output()
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Ensure the container system service is running, starting it (with kernel
/// install) if necessary. Returns whether the service is running afterwards.
async fn doctor_ensure_system_running(container_cli: &str, status_text: &str) -> bool {
    if status_text.contains("running") {
        println!("  status:  running ✓");
        return true;
    }
    println!("  status:  not running — starting (with kernel install)...");
    match tokio::process::Command::new(container_cli)
        .args(["system", "start", "--enable-kernel-install"])
        .status()
        .await
    {
        Ok(s) if s.success() => {
            println!("  status:  started ✓");
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

/// Ensure a default kernel is installed.
///
/// `container system kernel set` stores kernels under `<appRoot>/kernels/`,
/// which is not reflected in `container system property list`, so the directory
/// is checked directly using the `appRoot` reported by `system status`.
async fn doctor_ensure_kernel(container_cli: &str, status_text: &str) {
    // The status table format is: "appRoot            /path/to/dir/"
    let app_root = status_text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("appRoot")
            .map(|rest| std::path::PathBuf::from(rest.trim()))
    });

    let kernel_present = app_root.as_ref().is_some_and(|root| {
        let kernels_dir = root.join("kernels");
        std::fs::read_dir(&kernels_dir).is_ok_and(|mut d| d.next().is_some())
    });

    if kernel_present {
        println!("  kernel:  configured ✓");
        return;
    }
    println!("  kernel:  not configured — installing recommended kernel...");
    match tokio::process::Command::new(container_cli)
        .args(["system", "kernel", "set", "--recommended"])
        .status()
        .await
    {
        Ok(s) if s.success() => println!("  kernel:  installed ✓"),
        Ok(s) => {
            println!(
                "  kernel:  `{container_cli} system kernel set --recommended` failed (exit code: {:?})",
                s.code()
            );
            println!("           Run it manually for details.");
        }
        Err(e) => println!("  kernel:  failed to run: {e}"),
    }
}

/// If Rosetta is not installed, disable `build.rosetta` and reset the
/// `BuildKit` builder so cross-arch builds don't fail at bootstrap.
async fn doctor_ensure_rosetta_disabled(container_cli: &str) {
    let rosetta_installed = tokio::process::Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());

    if rosetta_installed {
        return;
    }

    // Disable Rosetta for builds via the property system.
    let prop_ok = tokio::process::Command::new(container_cli)
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

    // The BuildKit builder caches its launch config. Reset it so the next
    // build starts a fresh builder that reads the updated property.
    let stopped = tokio::process::Command::new(container_cli)
        .args(["builder", "stop"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    let deleted = tokio::process::Command::new(container_cli)
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
