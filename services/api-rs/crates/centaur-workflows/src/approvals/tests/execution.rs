use super::*;
use crate::approvals::execution::{Deadline, HostSandbox};
use async_trait::async_trait;
use centaur_sandbox_core::{
    ObservedSandbox, SandboxBackend, SandboxError, SandboxHandle, SandboxId, SandboxIo,
    SandboxResult, SandboxStatus,
};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn expired_claim_never_polls_executor() {
    let started = Instant::now();
    let deadline = Deadline::from_remaining(started, 1.0);
    tokio::time::sleep(Duration::from_secs(2)).await; // delayed claim commit/response
    let polled = AtomicBool::new(false);
    assert!(
        deadline
            .run(async {
                polled.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .is_err()
    );
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn provisioning_and_protocol_share_one_budget() {
    let started = Instant::now();
    let deadline = Deadline::from_remaining(started, 1.0);
    let result = deadline
        .run(async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            deadline.check()?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert_eq!(Instant::now() - started, Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn late_startup_is_stopped_without_dispatch() {
    let backend = Arc::new(SlowBackend::new(Duration::from_secs(2), false));
    let runtime = SandboxRuntime::backend(backend.clone(), SandboxSpec::new("fixture"));
    let mut host = HostSandbox::new(runtime);
    let deadline = Deadline::from_remaining(Instant::now(), 1.0);
    let dispatched = AtomicBool::new(false);
    assert!(
        deadline
            .run(async {
                let _io = host.start(SandboxSpec::new("fixture")).await?;
                deadline.check()?;
                dispatched.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .is_err()
    );
    assert!(!dispatched.load(Ordering::SeqCst));
    drop(host);
    backend.stopped.notified().await;
    assert!(!dispatched.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_startup_retains_cleanup_ownership() {
    let backend = Arc::new(SlowBackend::new(Duration::from_secs(2), false));
    let runtime = SandboxRuntime::backend(backend.clone(), SandboxSpec::new("fixture"));
    let mut host = HostSandbox::new(runtime);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            host.start(SandboxSpec::new("fixture"))
        )
        .await
        .is_err()
    );
    drop(host);
    backend.stopped.notified().await;
}

#[tokio::test(start_paused = true)]
async fn io_setup_failure_still_stops_created_sandbox() {
    let backend = Arc::new(SlowBackend::new(Duration::ZERO, true));
    let runtime = SandboxRuntime::backend(backend.clone(), SandboxSpec::new("fixture"));
    let mut host = HostSandbox::new(runtime);
    assert!(host.start(SandboxSpec::new("fixture")).await.is_err());
    backend.stopped.notified().await;
}

struct SlowBackend {
    delay: Duration,
    fail_io: bool,
    stopped: tokio::sync::Notify,
}

impl SlowBackend {
    fn new(delay: Duration, fail_io: bool) -> Self {
        Self {
            delay,
            fail_io,
            stopped: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl SandboxBackend for SlowBackend {
    fn name(&self) -> &'static str {
        "approval-deadline-fixture"
    }
    async fn create(&self, _spec: SandboxSpec) -> SandboxResult<SandboxHandle> {
        tokio::time::sleep(self.delay).await;
        Ok(SandboxHandle {
            id: SandboxId::new("fixture"),
            backend: self.name().into(),
        })
    }
    async fn open_io(&self, _id: &SandboxId) -> SandboxResult<SandboxIo> {
        if self.fail_io {
            return Err(SandboxError::backend("fixture I/O failure"));
        }
        Ok(SandboxIo::new(
            Box::pin(tokio::io::sink()),
            Box::pin(tokio::io::empty()),
            Box::pin(tokio::io::empty()),
        ))
    }
    async fn stop(&self, _id: &SandboxId) -> SandboxResult<()> {
        self.stopped.notify_one();
        Ok(())
    }
    async fn status(&self, _id: &SandboxId) -> SandboxResult<SandboxStatus> {
        unreachable!()
    }
    async fn observe(&self, _id: &SandboxId) -> SandboxResult<ObservedSandbox> {
        unreachable!()
    }
    async fn list_observed(&self) -> SandboxResult<Vec<ObservedSandbox>> {
        unreachable!()
    }
    async fn pause(&self, _id: &SandboxId) -> SandboxResult<()> {
        unreachable!()
    }
    async fn resume(&self, _id: &SandboxId) -> SandboxResult<()> {
        unreachable!()
    }
}
