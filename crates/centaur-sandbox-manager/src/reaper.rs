//! Background garbage collection for leaked sandboxes.
//!
//! Sessions pause idle sandboxes (suspending them), but paused sandboxes and
//! sandboxes whose sessions never go idle still need a restart-surviving
//! backstop. The reaper sweeps the backend's observed sandboxes and stops any
//! that exceed the configured max lifetime, releasing the sandbox, its proxy
//! resources, and its node pod slots. That includes sandboxes whose agent
//! exited (`Stopped`): sessions replace rather than reuse them, so nothing
//! else ever releases their backend resources. A warm-pool sandbox's lifetime
//! starts when a session claims it, not when the pool created it, and a
//! sandbox whose session has a queued or running execution is left for a later
//! sweep unless that execution has itself outlived the max lifetime. Each sweep also deletes iron-proxy
//! resources whose sandbox no longer has a live Sandbox, the orphan class no
//! observed-sandbox path can reach.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use centaur_sandbox_core::{
    COMPONENT_LABEL, ObservedSandbox, SandboxResult, SandboxStatus, WORKFLOW_RUN_COMPONENT,
    WORKFLOW_RUN_ID_LABEL,
};
use centaur_session_sqlx::{PgSessionStore, SandboxReapState};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};

use crate::SandboxManager;

const WORKFLOW_ORPHAN_MIN_AGE: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone, Copy, Debug)]
pub struct SandboxReaperConfig {
    /// How often to sweep.
    pub interval: Duration,
    /// Minimum age of an iron-proxy resource whose Sandbox no longer exists
    /// before the orphan sweep may delete it.
    pub orphan_sweep_grace: Duration,
    /// Stop any idle sandbox older than this, measured from its warm-pool
    /// claim when it has one. `None` disables the max-lifetime sweep.
    pub max_lifetime: Option<Duration>,
}

impl SandboxReaperConfig {
    /// The orphan sweep runs whenever a sweep interval is configured, so the
    /// reaper is enabled even when the max-lifetime sweep is disabled.
    pub fn is_enabled(&self) -> bool {
        self.interval > Duration::ZERO || self.max_lifetime.is_some()
    }
}

pub struct SandboxReaper {
    manager: Arc<SandboxManager>,
    store: PgSessionStore,
    config: SandboxReaperConfig,
}

impl SandboxReaper {
    pub fn new(
        manager: Arc<SandboxManager>,
        store: PgSessionStore,
        config: SandboxReaperConfig,
    ) -> Self {
        Self {
            manager,
            store,
            config,
        }
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            // Orphans left by a dead control plane are reached sooner when
            // the first sweep runs at startup rather than after the interval.
            if let Err(error) = self.reap_once().await {
                warn!(%error, "initial sandbox reaper sweep failed");
            }
            let mut tick = interval(self.config.interval);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(error) = self.reap_once().await {
                    warn!(%error, "sandbox reaper sweep failed");
                }
            }
        });
    }

    /// Sweep once and return how many sandboxes were stopped. A failed stop is
    /// logged and skipped so one wedged sandbox cannot stall the sweep.
    pub async fn reap_once(&self) -> SandboxResult<usize> {
        let now = SystemTime::now();
        let mut reaped = 0;
        for observed in self.manager.list_observed().await? {
            // Workflow hosts have no session or warm-pool row. Check attempt
            // ownership even when a host never finished starting.
            if observed.labels.get(COMPONENT_LABEL).map(String::as_str)
                == Some(WORKFLOW_RUN_COMPONENT)
            {
                match observed.status {
                    SandboxStatus::Stopped => {
                        reaped += self.stop(&observed, "workflow_stopped").await;
                        continue;
                    }
                    SandboxStatus::Created | SandboxStatus::Running | SandboxStatus::Unknown(_) => {
                        if self.workflow_host_is_orphan(&observed, now).await {
                            reaped += self.stop(&observed, "workflow_orphan").await;
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            // Durable state can only defer a reap, so skip the lookup for
            // sandboxes that are not expired by backend age alone.
            if reap_reason(&observed, &SandboxReapState::default(), now, &self.config).is_none() {
                continue;
            }
            let state = match self.store.sandbox_reap_state(observed.id.as_str()).await {
                Ok(state) => state,
                Err(error) => {
                    warn!(
                        sandbox_id = %observed.id.as_str(),
                        %error,
                        "failed to load sandbox reap state; skipping"
                    );
                    continue;
                }
            };
            let Some(reason) = reap_reason(&observed, &state, now, &self.config) else {
                if state.active_execution_since.is_some() {
                    info!(
                        sandbox_id = %observed.id.as_str(),
                        "deferring reap of expired sandbox with an active execution"
                    );
                }
                continue;
            };
            // A sandbox read as unclaimed may be a ready warm sandbox that a
            // session is claiming right now. Take it out of the pool first and
            // leave it alone if the claim won; the replenisher finalizes the
            // evicted row once the sandbox is gone.
            if state.claimed_at.is_none() {
                match self
                    .store
                    .evict_unclaimed_warm_sandbox(observed.id.as_str())
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        warn!(
                            sandbox_id = %observed.id.as_str(),
                            %error,
                            "failed to evict warm sandbox before reaping; skipping"
                        );
                        continue;
                    }
                }
            }
            reaped += self.stop(&observed, reason).await;
        }
        let orphaned = self
            .manager
            .reap_orphan_iron_proxy_resources(self.config.orphan_sweep_grace)
            .await?;
        if !orphaned.is_empty() {
            info!(?orphaned, "reaped orphaned iron-proxy resources");
        }
        Ok(reaped)
    }

    async fn workflow_host_is_orphan(&self, observed: &ObservedSandbox, now: SystemTime) -> bool {
        let age = observed
            .created_at
            .and_then(|created| now.duration_since(created).ok());
        if age.is_none_or(|age| age < WORKFLOW_ORPHAN_MIN_AGE) {
            return false;
        }
        // Every dispatched host carries an attempt ID. Its worker can renew
        // the claim indefinitely, including from another API replica.
        let Some(run_id) = observed.labels.get(WORKFLOW_RUN_ID_LABEL) else {
            return true;
        };
        match self.store.workflow_run_has_live_claim(run_id).await {
            Ok(live) => !live,
            Err(error) => {
                warn!(
                    sandbox_id = %observed.id.as_str(),
                    %error,
                    "failed to check workflow claim; skipping"
                );
                false
            }
        }
    }

    async fn stop(&self, observed: &ObservedSandbox, reason: &'static str) -> usize {
        match self.manager.stop(&observed.id).await {
            Ok(()) => {
                info!(
                    sandbox_id = %observed.id.as_str(),
                    reason,
                    "reaped sandbox"
                );
                1
            }
            Err(error) => {
                warn!(
                    sandbox_id = %observed.id.as_str(),
                    reason,
                    %error,
                    "failed to reap sandbox"
                );
                0
            }
        }
    }
}

fn reap_reason(
    observed: &ObservedSandbox,
    state: &SandboxReapState,
    now: SystemTime,
    config: &SandboxReaperConfig,
) -> Option<&'static str> {
    let max_lifetime = config.max_lifetime?;
    let expired = |since: Option<SystemTime>| {
        since.is_some_and(|since| {
            now.duration_since(since)
                .is_ok_and(|age| age >= max_lifetime)
        })
    };
    if observed.status == SandboxStatus::Gone || !expired(state.claimed_at.or(observed.created_at))
    {
        return None;
    }
    match state.active_execution_since {
        None => Some("max_lifetime"),
        // An active turn defers the reap, but a turn that itself outlived the
        // max lifetime is treated as stuck so the backstop still holds.
        Some(since) if expired(Some(since)) => Some("active_execution_exceeded_max_lifetime"),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use centaur_sandbox_core::{SandboxBackend, SandboxHandle, SandboxId, SandboxIo, SandboxSpec};
    use std::{collections::BTreeMap, sync::Mutex};

    fn config(max_lifetime: Option<Duration>) -> SandboxReaperConfig {
        SandboxReaperConfig {
            interval: Duration::from_secs(60),
            orphan_sweep_grace: Duration::from_secs(600),
            max_lifetime,
        }
    }

    fn observed(status: centaur_sandbox_core::SandboxStatus) -> ObservedSandbox {
        ObservedSandbox::new("sandbox-1", "fake", status)
    }

    #[test]
    fn reaps_running_sandbox_past_max_lifetime() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Running)
            .with_created_at(Some(now - Duration::from_secs(100_000)));

        let reason = reap_reason(
            &sandbox,
            &SandboxReapState::default(),
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, Some("max_lifetime"));
    }

    #[test]
    fn reaps_suspended_sandbox_past_max_lifetime() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Suspended)
            .with_created_at(Some(now - Duration::from_secs(100_000)))
            .with_suspended_since(Some(now - Duration::from_secs(60)));

        let reason = reap_reason(
            &sandbox,
            &SandboxReapState::default(),
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, Some("max_lifetime"));
    }

    #[test]
    fn keeps_running_sandbox_within_max_lifetime() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Running)
            .with_created_at(Some(now - Duration::from_secs(60)));

        let reason = reap_reason(
            &sandbox,
            &SandboxReapState::default(),
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn reaps_stopped_sandbox_past_max_lifetime() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Stopped)
            .with_created_at(Some(now - Duration::from_secs(100_000)));

        let reason = reap_reason(
            &sandbox,
            &SandboxReapState::default(),
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, Some("max_lifetime"));
    }

    #[test]
    fn ignores_gone_sandboxes() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Gone)
            .with_created_at(Some(now - Duration::from_secs(100_000)));

        let reason = reap_reason(
            &sandbox,
            &SandboxReapState::default(),
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn disabled_max_lifetime_reaps_nothing_by_age() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Suspended)
            .with_created_at(Some(now - Duration::from_secs(100_000)))
            .with_suspended_since(Some(now - Duration::from_secs(100_000)));
        let config = config(None);

        assert_eq!(
            reap_reason(&sandbox, &SandboxReapState::default(), now, &config),
            None
        );
    }

    #[test]
    fn warm_sandbox_lifetime_starts_at_claim() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Running)
            .with_created_at(Some(now - Duration::from_secs(100_000)));
        let state = SandboxReapState {
            claimed_at: Some(now - Duration::from_secs(60)),
            active_execution_since: None,
        };

        let reason = reap_reason(
            &sandbox,
            &state,
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn keeps_expired_sandbox_with_active_execution() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Running)
            .with_created_at(Some(now - Duration::from_secs(100_000)));
        let state = SandboxReapState {
            claimed_at: Some(now - Duration::from_secs(100_000)),
            active_execution_since: Some(now - Duration::from_secs(3_600)),
        };

        let reason = reap_reason(
            &sandbox,
            &state,
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn reaps_sandbox_whose_active_execution_outlived_max_lifetime() {
        let now = SystemTime::now();
        let sandbox = observed(centaur_sandbox_core::SandboxStatus::Running)
            .with_created_at(Some(now - Duration::from_secs(100_000)));
        let state = SandboxReapState {
            claimed_at: None,
            active_execution_since: Some(now - Duration::from_secs(90_000)),
        };

        let reason = reap_reason(
            &sandbox,
            &state,
            now,
            &config(Some(Duration::from_secs(86_400))),
        );

        assert_eq!(reason, Some("active_execution_exceeded_max_lifetime"));
    }

    #[test]
    fn orphan_sweep_keeps_the_reaper_enabled_without_max_lifetime() {
        let config = config(None);
        assert!(config.is_enabled());
    }

    struct SweepBackend {
        sandboxes: Vec<ObservedSandbox>,
        stopped: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SandboxBackend for SweepBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        async fn list_observed(&self) -> SandboxResult<Vec<ObservedSandbox>> {
            Ok(self.sandboxes.clone())
        }
        async fn stop(&self, id: &SandboxId) -> SandboxResult<()> {
            self.stopped.lock().unwrap().push(id.as_str().to_owned());
            Ok(())
        }
        async fn create(&self, _: SandboxSpec) -> SandboxResult<SandboxHandle> {
            unreachable!()
        }
        async fn open_io(&self, _: &SandboxId) -> SandboxResult<SandboxIo> {
            unreachable!()
        }
        async fn status(&self, _: &SandboxId) -> SandboxResult<SandboxStatus> {
            unreachable!()
        }
        async fn observe(&self, _: &SandboxId) -> SandboxResult<ObservedSandbox> {
            unreachable!()
        }
        async fn pause(&self, _: &SandboxId) -> SandboxResult<()> {
            unreachable!()
        }
        async fn resume(&self, _: &SandboxId) -> SandboxResult<()> {
            unreachable!()
        }
    }

    fn host(id: &str, status: SandboxStatus, created: Option<SystemTime>) -> ObservedSandbox {
        ObservedSandbox::new(id, "test", status)
            .with_created_at(created)
            .with_labels(BTreeMap::from([(
                COMPONENT_LABEL.into(),
                WORKFLOW_RUN_COMPONENT.into(),
            )]))
    }

    async fn sweep(store: PgSessionStore, sandboxes: Vec<ObservedSandbox>) -> Vec<String> {
        let backend = Arc::new(SweepBackend {
            sandboxes,
            stopped: Mutex::default(),
        });
        let reaper = SandboxReaper::new(
            Arc::new(SandboxManager::new(backend.clone())),
            store,
            config(Some(Duration::from_secs(72 * 60 * 60))),
        );
        let count = reaper.reap_once().await.unwrap();
        let stopped = backend.stopped.lock().unwrap().clone();
        assert_eq!(count, stopped.len());
        stopped
    }

    #[tokio::test]
    async fn sweep_reaps_stopped_workflows_and_scopes_the_running_age_limit() {
        let now = SystemTime::now();
        let old = Some(now - WORKFLOW_ORPHAN_MIN_AGE);
        let mut session = host("session", SandboxStatus::Stopped, old);
        session
            .labels
            .insert(COMPONENT_LABEL.into(), "session-sandbox".into());
        let mut needs_lookup = host("unknown-claim", SandboxStatus::Running, old);
        needs_lookup
            .labels
            .insert(WORKFLOW_RUN_ID_LABEL.into(), "attempt".into());
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/test").unwrap();
        pool.close().await;
        let mut running_session = session.clone();
        running_session.status = SandboxStatus::Running;
        assert_eq!(
            sweep(
                PgSessionStore::new(pool),
                vec![
                    session,
                    running_session,
                    needs_lookup,
                    host("stopped", SandboxStatus::Stopped, None),
                    host("old-unowned", SandboxStatus::Running, old),
                    host("young", SandboxStatus::Running, Some(now)),
                    host("undated", SandboxStatus::Running, None),
                    host(
                        "future",
                        SandboxStatus::Running,
                        Some(now + WORKFLOW_ORPHAN_MIN_AGE)
                    ),
                    host("gone", SandboxStatus::Gone, old),
                ]
            )
            .await,
            ["stopped", "old-unowned"]
        );
    }

    #[tokio::test]
    async fn sweep_reaps_stuck_workflow_hosts_only_after_the_grace_period() {
        let now = SystemTime::now();
        let old = Some(now - WORKFLOW_ORPHAN_MIN_AGE - Duration::from_secs(1));
        let unknown = SandboxStatus::Unknown("ImagePullBackOff".into());
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/test").unwrap();
        pool.close().await;
        assert_eq!(
            sweep(
                PgSessionStore::new(pool),
                vec![
                    host("old-created", SandboxStatus::Created, old),
                    host("old-unknown", unknown.clone(), old),
                    host("new-created", SandboxStatus::Created, Some(now)),
                    host("new-unknown", unknown, Some(now)),
                ],
            )
            .await,
            ["old-created", "old-unknown"]
        );
    }

    #[tokio::test]
    async fn sweep_preserves_live_workflow_hosts_and_reaps_after_claim_expiry() {
        let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
            eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
            return;
        };
        let store = PgSessionStore::connect(&url).await.unwrap();
        store
            .run_migrations(centaur_session_sqlx::TextSearchBackend::Postgres)
            .await
            .unwrap();
        let run_id: String = sqlx::query_scalar(
            "insert into absurd.r_centaur_workflows_etl_backfill
                (run_id, task_id, attempt, state, claimed_by, claim_expires_at, available_at)
             values (gen_random_uuid(), gen_random_uuid(), 1, 'running', 'test', now() + interval '5 minutes', now())
             returning run_id::text",
        ).fetch_one(store.pool()).await.unwrap();
        let old = Some(SystemTime::now() - WORKFLOW_ORPHAN_MIN_AGE);
        let mut sandboxes = vec![
            host("running", SandboxStatus::Running, old),
            host("created", SandboxStatus::Created, old),
            host("unknown", SandboxStatus::Unknown("Pending".into()), old),
        ];
        for sandbox in &mut sandboxes {
            sandbox
                .labels
                .insert(WORKFLOW_RUN_ID_LABEL.into(), run_id.clone());
        }
        assert!(sweep(store.clone(), sandboxes.clone()).await.is_empty());
        sqlx::query("update absurd.r_centaur_workflows_etl_backfill set claim_expires_at = now() - interval '1 second' where run_id = $1::text::uuid")
            .bind(&run_id).execute(store.pool()).await.unwrap();
        assert_eq!(
            sweep(store.clone(), sandboxes.clone()).await,
            ["running", "created", "unknown"]
        );
        sqlx::query(
            "delete from absurd.r_centaur_workflows_etl_backfill where run_id = $1::text::uuid",
        )
        .bind(&run_id)
        .execute(store.pool())
        .await
        .unwrap();
        assert_eq!(
            sweep(store, sandboxes).await,
            ["running", "created", "unknown"]
        );
    }
}
