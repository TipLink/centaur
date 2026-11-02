//! One claim budget for provisioning and dispatch, with durable completion before cleanup.
use super::*;
use centaur_sandbox_core::{SandboxId, SandboxIoParts};
use centaur_session_runtime::SessionRuntimeError;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(super) fn from_remaining(started: Instant, seconds: f64) -> Self {
        Self(started + Duration::from_secs_f64(seconds.max(0.0)))
    }

    pub(crate) fn check(self) -> Result<(), WorkflowRuntimeError> {
        if Instant::now() >= self.0 {
            return Err(uncertain());
        }
        Ok(())
    }

    pub(crate) async fn run<T>(
        self,
        future: impl Future<Output = Result<T, WorkflowRuntimeError>>,
    ) -> Result<T, WorkflowRuntimeError> {
        self.check()?;
        let result = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.0) => return Err(uncertain()),
            result = future => result,
        };
        // Also reject a future that completed without yielding across the deadline.
        self.check()?;
        result
    }

    async fn cleanup(self, future: impl Future<Output = ()>) {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.0) => {},
            () = future => {},
        }
    }
}

fn uncertain() -> WorkflowRuntimeError {
    WorkflowRuntimeError::Upstream("approved workflow outcome uncertain".into())
}

pub(crate) struct ClaimedExecution<'a> {
    pub repo: &'a Repository,
    pub id: Uuid,
    pub policy: &'a ActionPolicy,
    pub deadline: Deadline,
}

impl ClaimedExecution<'_> {
    pub(crate) async fn complete(
        self,
        result: Result<Value, WorkflowRuntimeError>,
        cleanup: impl Future<Output = ()>,
    ) -> Result<Value, WorkflowRuntimeError> {
        // Never let slow shutdown hide a known result from lifecycle reconciliation.
        // The database independently rejects success persisted after its deadline.
        let saved = self.repo.finish(self.id, self.policy, result).await;
        self.deadline.cleanup(cleanup).await;
        saved?;
        Ok(Value::Null)
    }
}

type Startup = JoinHandle<Result<(SandboxId, SandboxIoParts), SessionRuntimeError>>;

/// Keep ownership of startup even if the caller's deadline expires. A late
/// sandbox is stopped without ever receiving workflow.start. Dropping a backend
/// create future could otherwise lose its ID and leak the privileged sandbox.
pub(crate) struct HostSandbox {
    runtime: SandboxRuntime,
    startup: Option<Startup>,
    id: Option<SandboxId>,
}

impl HostSandbox {
    pub(crate) fn new(runtime: SandboxRuntime) -> Self {
        Self {
            runtime,
            startup: None,
            id: None,
        }
    }

    pub(crate) async fn start(
        &mut self,
        spec: SandboxSpec,
    ) -> Result<SandboxIoParts, WorkflowRuntimeError> {
        let runtime = self.runtime.clone();
        self.startup = Some(tokio::spawn(async move {
            runtime.create_running_io(spec).await
        }));
        let result = self.startup.as_mut().expect("startup task installed").await;
        self.startup = None;
        let (id, io) = result.map_err(|error| {
            WorkflowRuntimeError::Internal(format!("workflow sandbox startup failed: {error}"))
        })??;
        self.id = Some(id);
        Ok(io)
    }

    fn spawn_cleanup(&mut self) -> Option<JoinHandle<()>> {
        let startup = self.startup.take();
        let id = self.id.take();
        if startup.is_none() && id.is_none() {
            return None;
        }
        let runtime = self.runtime.clone();
        Some(tokio::spawn(async move {
            let id = match (id, startup) {
                (Some(id), _) => id,
                (_, Some(startup)) => match startup.await {
                    Ok(Ok((id, io))) => {
                        drop(io);
                        id
                    }
                    _ => return,
                },
                _ => return,
            };
            if let Err(error) = runtime.stop_sandbox(&id).await {
                warn!(sandbox_id = %id.as_str(), %error, "failed to stop workflow host sandbox");
            }
        }))
    }

    pub(crate) async fn cleanup(&mut self) {
        if let Some(task) = self.spawn_cleanup() {
            // Dropping this join handle at the deadline leaves cleanup running.
            let _ = task.await;
        }
    }
}

impl Drop for HostSandbox {
    fn drop(&mut self) {
        // Also covers cancellation during setup, protocol execution, or persistence.
        self.spawn_cleanup();
    }
}
