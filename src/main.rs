//! `npxc` binary entry-point.
//!
//! Module declarations live in `lib.rs`; this file references the library
//! crate as `npxc::` — the standard pattern for a mixed bin+lib Rust crate.

use clap::Parser;

use npxc::{
    cli::{Cli, Commands, GlobalOpts},
    config::{self, NetworkPolicy},
    error::NpxcError,
    rpc::pipeline,
    runtime::{
        LaunchPlan, ManagedNetwork, Mount, MountMode, Session, ensure_image, image_tag,
        list_images, remove_image,
    },
    tunnel,
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
/// `args[1..]` (with bare `--` separators stripped) are forwarded verbatim to
/// the container's entrypoint as the package's own arguments.
async fn run_package(args: Vec<String>, global: GlobalOpts) -> Result<(), NpxcError> {
    let pkg_spec = args.first().cloned().unwrap_or_default();
    // Filter bare "--" separators; forward remaining args to the container.
    let pkg_args: Vec<String> = args[1..].iter().filter(|a| *a != "--").cloned().collect();

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
        println!("network:    {}", effective.network);
        println!("no_isolate: {}", global.no_isolate);
        println!("args:       {pkg_args:?}");
        return Ok(());
    }

    // Build the image if it does not already exist.
    let tag = ensure_image(&pkg_name, &version, &effective, false).await?;

    // Persist the resolved version (best-effort; log but don't fail).
    if let Err(e) = config::ensure_version_pinned(&pkg_name, &version, config_path) {
        tracing::error!("failed to pin version for {pkg_name}@{version}: {e}");
    }

    let mut plan = LaunchPlan::build(&pkg_name, &effective, &cwd, pkg_args, global.no_isolate)?;

    // Provision the per-session network (creates an isolated `--internal`
    // network for allowlist mode; a no-op otherwise) before launching.
    let (network_arg, managed_network) =
        ManagedNetwork::provision(&effective.network, &effective.container_cli).await?;

    // For allowlist mode, stand up the egress tunnel on the network's gateway
    // and inject its config + the NET_ADMIN capability the guest needs to bring
    // up `wg0`. The returned tunnel must outlive the container, so it is held
    // in `_tunnel` until the session finishes below.
    let _tunnel = match (&effective.network, &managed_network) {
        (NetworkPolicy::Allowlist { .. }, Some(net)) => {
            let gateway = net.gateway.parse().map_err(|e| {
                NpxcError::Runtime(format!("invalid gateway address {:?}: {e}", net.gateway))
            })?;
            let setup = tunnel::establish(gateway).await?;
            plan.env_literal.extend(setup.env.iter().cloned());
            // Mount npxc's resolv.conf so the guest resolves through the tunnel
            // rather than the host's (possibly host-only-unreachable) resolver.
            plan.mounts.push(Mount {
                host: setup.resolv_conf.path().to_path_buf(),
                container: "/etc/resolv.conf".to_string(),
                mode: MountMode::Ro,
            });
            // NET_ADMIN: configure wg0. SETUID/SETGID: drop root → node after
            // setup. All three are used only by the trusted entrypoint; the
            // server process ends up unprivileged with no capabilities.
            for cap in ["NET_ADMIN", "SETUID", "SETGID"] {
                plan.cap_add.push(cap.to_string());
            }
            Some(setup)
        }
        _ => None,
    };

    let mut session = match Session::start(&pkg_name, &tag, &effective, &network_arg, &plan, None) {
        Ok(session) => session,
        Err(e) => {
            // The session never took ownership of the network; clean it up.
            if let Some(net) = managed_network {
                net.delete_blocking();
            }
            return Err(e);
        }
    };
    session.attach_network(managed_network);

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
    if let NetworkPolicy::Allowlist { allow } = &effective.network {
        for entry in allow {
            println!("  allow:       {entry}");
        }
    }
    println!("memory:        {}", effective.memory);
    println!("cpus:          {}", effective.cpus);
    println!("mount_mode:    {}", effective.mount_mode);
    println!("strategies:    {:?}", effective.strategies);

    // Env grant sheet: key names for literals, variable names for passthrough.
    if !effective.env.is_empty() {
        let mut pairs: Vec<_> = effective.env.keys().collect();
        pairs.sort();
        println!("env:           {pairs:?}");
    }
    if !effective.env_passthrough.is_empty() {
        println!("env_passthrough: {:?}", effective.env_passthrough);
    }

    // Storage and mount summary.
    if let Some(storage) = &effective.storage {
        if storage.persist {
            println!("storage:       persist → /data (rw)");
        }
        if !storage.writable.is_empty() {
            println!("storage.writable: {:?}", storage.writable);
        }
    }
    if !effective.mounts.is_empty() {
        for mc in &effective.mounts {
            println!(
                "mount:         {} → {} ({})",
                mc.host, mc.container, mc.mode
            );
        }
    }

    Ok(())
}

/// Check prerequisites: verify the container runtime is available, print its
/// version, and ensure the VM kernel is configured.
async fn cmd_doctor(global: GlobalOpts) -> Result<(), NpxcError> {
    let cfg = config::load_global_config(global.config.as_ref())?;
    npxc::doctor::run(&cfg.defaults.container_cli).await;
    Ok(())
}
