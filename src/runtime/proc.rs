use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

/// A running container process with its stdio handles taken out of the child.
///
/// Splitting the handles from the `Child` lets callers use them independently
/// (e.g. for async read/write loops) while still being able to wait for or
/// kill the process.
pub struct ContainerProcess {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

impl ContainerProcess {
    /// Take I/O handles from `child` and return a `ContainerProcess`.
    ///
    /// # Panics
    /// Panics if `child` was spawned without `Stdio::piped()` for all three
    /// standard streams.
    #[must_use]
    pub fn from_child(mut child: Child) -> Self {
        let stdin = child
            .stdin
            .take()
            .expect("ContainerProcess: child stdin not piped");
        let stdout = child
            .stdout
            .take()
            .expect("ContainerProcess: child stdout not piped");
        let stderr = child
            .stderr
            .take()
            .expect("ContainerProcess: child stderr not piped");
        ContainerProcess {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    /// Return the child process ID, if the process is still alive and the OS
    /// supports it.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Send SIGKILL to the container process. Non-blocking — does not wait for
    /// the process to exit. Errors (e.g. the process already exited) are
    /// silently ignored.
    pub fn kill_now(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Send SIGKILL and wait until the process has exited.
    pub async fn kill_and_wait(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}
