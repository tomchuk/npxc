pub mod identify;
pub mod publish;
pub mod validate;

pub use identify::identify_path_args;
pub use publish::{PublicationCache, PublicationKey, PublishedFile, publish_file};
pub use validate::validate_path;

use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;

use crate::rpc::message::ToolSchema;

/// Shared state for a single `npxc` session.
///
/// Both pipeline tasks (client→server and server→client) hold an
/// `Arc<SessionState>`. Fields are individually wrapped in `Arc<Mutex<…>>`
/// so each can be locked independently and for the shortest possible
/// critical section — never across an `.await` point. `parking_lot::Mutex`
/// is used so locks cannot be poisoned by a panicking task.
#[derive(Debug)]
pub struct SessionState {
    /// Forward map: `(canonical host path, mtime_nanos)` → published file.
    ///
    /// **Writer**: the client→server task (path publication step).
    /// **Readers**: both tasks — the client→server task for deduplication,
    /// the server→client task for the reverse lookup during response translation.
    pub publications: Arc<Mutex<PublicationCache>>,

    /// Cached tool input schemas keyed by tool name, populated from
    /// `tools/list` responses.
    ///
    /// **Writer**: the server→client task (on `tools/list` response).
    /// **Reader**: the client→server task (on `tools/call`, schema strategy).
    pub tool_schemas: Arc<Mutex<HashMap<String, ToolSchema>>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            publications: Arc::new(Mutex::new(PublicationCache::new())),
            tool_schemas: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
