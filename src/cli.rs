use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Sandboxed npm execution for MCP servers
#[derive(Debug, Parser)]
#[command(
    name = "npxc",
    about = "Sandboxed npm execution for MCP servers",
    version
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// Alternate config file path
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Override CWD scope
    #[arg(long, global = true, value_name = "PATH")]
    pub cwd: Option<PathBuf>,

    /// Disable filesystem scoping
    #[arg(long, global = true)]
    pub no_isolate: bool,

    /// Log level: trace|debug|info|warn|error (also reads `NPXC_LOG` env var)
    #[arg(
        long,
        global = true,
        value_name = "LEVEL",
        default_value = "warn",
        env = "NPXC_LOG"
    )]
    pub log_level: String,

    /// Print plan and exit without executing
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Accept (and ignore) the `-y`/`--yes` flag emitted by MCP client
    /// configs written as `npx -y <pkg> …`.  When `npxc` is used as a
    /// drop-in replacement, clap would otherwise reject the leading flag.
    #[arg(short = 'y', long = "yes", global = true, hide = true)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Build the container image for a package without running it
    Build {
        /// Package specification (e.g. @scope/pkg or @scope/pkg@1.2.3)
        package_spec: String,
    },

    /// Force a --no-cache rebuild of the container image
    Rebuild {
        /// Package specification (e.g. @scope/pkg or @scope/pkg@1.2.3)
        package_spec: String,
    },

    /// List all cached container images managed by npxc
    List,

    /// Remove one or all cached container images
    Clean {
        /// Package specification to remove (omit with --all to remove everything)
        package_spec: Option<String>,

        /// Remove all cached images
        #[arg(long)]
        all: bool,
    },

    /// Print resolved config, image, and mount information, then exit
    Inspect {
        /// Package specification (e.g. @scope/pkg or @scope/pkg@1.2.3)
        package_spec: String,
    },

    /// Check prerequisites (container runtime availability, etc.)
    Doctor,

    /// Run a package in a sandboxed container (default mode).
    ///
    /// The first element of the captured arguments is the package spec;
    /// remaining elements (minus any bare `--` separator) are forwarded to
    /// the package as its arguments.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
