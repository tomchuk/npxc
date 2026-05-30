use thiserror::Error;

#[derive(Debug, Error)]
pub enum NpxcError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Container runtime not available: {0}")]
    RuntimeNotAvailable(String),

    #[error("Image build failed (exit code: {})", .code.map_or_else(|| "unknown".to_string(), |c| c.to_string()))]
    BuildFailed { code: Option<i32> },

    #[error("Runtime error: {0}")]
    Runtime(String),

    #[error("Path outside CWD scope: {path} (cwd: {cwd})")]
    PathOutOfScope { path: String, cwd: String },

    #[error("Path not found or unreadable: {0}")]
    PathNotFound(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),

    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl NpxcError {
    /// Map to the CLI exit code defined in the spec.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            NpxcError::RuntimeNotAvailable(_) => 2,
            NpxcError::BuildFailed { .. } => 3,
            NpxcError::Runtime(_) => 4,
            // Config, TomlDe, TomlSer, and every other variant map to the
            // generic error exit code.
            _ => 1,
        }
    }

    /// If this error should produce a JSON-RPC error response back to the
    /// client (rather than propagating as a process error), return the
    /// serialized response line.
    #[must_use]
    pub fn to_rpc_error_response(&self, id: &serde_json::Value) -> Option<String> {
        match self {
            NpxcError::PathOutOfScope { path, cwd } => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": "npxc: path outside CWD scope",
                        "data": { "path": path, "cwd": cwd }
                    }
                });
                Some(resp.to_string())
            }
            NpxcError::PathNotFound(path) => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": "npxc: path not found or unreadable",
                        "data": { "path": path }
                    }
                });
                Some(resp.to_string())
            }
            _ => None,
        }
    }
}
