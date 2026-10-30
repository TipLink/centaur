use super::*;

#[tokio::test]
async fn native_admission_decision_claim_and_recovery() {
    let Ok(url) = env::var("TOOL_APPROVAL_TEST_DATABASE_URL") else {
        return;
    };
    let admin = PgPool::connect(&url).await.unwrap();
    let installed: bool =
        sqlx::query_scalar("select exists(select 1 from pg_namespace where nspname='absurd')")
            .fetch_one(&admin)
            .await
            .unwrap();
    if !installed {
        for sql in [
            include_str!("../../../../centaur-session-sqlx/migrations/0007_absurd_workflows.sql"),
            include_str!(
                "../../../../centaur-session-sqlx/migrations/0009_absurd_await_event_task_guard.sql"
            ),
        ] {
            sqlx::raw_sql(sql).execute(&admin).await.unwrap();
        }
    }
    let schema = format!("approval_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create schema {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let search = format!("set search_path to {schema}, public");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let search = search.clone();
            Box::pin(async move {
                sqlx::query(&search).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    for sql in [
        include_str!("../../../../centaur-session-sqlx/migrations/0001_session_control_plane.sql"),
        include_str!(
            "../../../../centaur-session-sqlx/migrations/0003_session_iron_control_principal.sql"
        ),
        include_str!("../../../../centaur-session-sqlx/migrations/0056_tool_approvals.sql"),
        include_str!(
            "../../../../centaur-session-sqlx/migrations/0057_workflow_native_approvals.sql"
        ),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    let client = Client::from_pool_with_options(
        pool.clone(),
        ClientOptions {
            queue_name: WORKFLOW_QUEUE.into(),
            ..Default::default()
        },
    )
    .unwrap();
    client.create_queue(None, Default::default()).await.unwrap();
    // Enqueue participates in the caller's transaction, not a separate commit.
    let mut tx = pool.begin().await.unwrap();
    let rolled_back = client
        .spawn_with_executor(
            WORKFLOW_TASK,
            json!({"workflow_name":DRIVER_WORKFLOW}),
            SpawnOptions {
                queue: Some(WORKFLOW_QUEUE.into()),
                ..Default::default()
            },
            &mut *tx,
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "select count(*) from absurd.t_centaur_workflows where task_id=$1::uuid",
    )
    .bind(rolled_back.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let event = format!("rollback:{}", Uuid::new_v4());
    let mut tx = pool.begin().await.unwrap();
    client
        .emit_event_with_executor(&event, json!({"decision":"approved"}), None, &mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 =
        sqlx::query_scalar("select count(*) from absurd.e_centaur_workflows where event_name=$1")
            .bind(&event)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let mut tx = pool.begin().await.unwrap();
    client
        .emit_event_with_executor(&event, json!({"decision":"approved"}), None, &mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    client
        .emit_event(&event, json!({"decision":"declined"}), None)
        .await
        .unwrap();
    let emitted: Value =
        sqlx::query_scalar("select payload from absurd.e_centaur_workflows where event_name=$1")
            .bind(&event)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(emitted, json!({"decision":"approved"}));
    sqlx::query("insert into sessions(thread_key,sandbox_id,harness_type,status,iron_control_principal) values('slack:C1:1.000','sandbox-test','codex','active','prn_requester')").execute(&pool).await.unwrap();
    sqlx::query("insert into session_executions(execution_id,thread_key,status,metadata) values('exe_test','slack:C1:1.000','running',$1)")
        .bind(json!({"source":"slackbotv2","platform":"slack","slack_team_id":"T1","slack_home_team_id":"T1","slack_channel_id":"C1","slack_user_id":"U2"})).execute(&pool).await.unwrap();
    let mut p = policy();
    p.approver_user_ids.extend(["U2".into(), "U3".into()]);
    p.public_result_fields = vec!["message".into()];
    let repo = Repository {
        pool: pool.clone(),
        policies: Arc::new(BTreeMap::from([("action".into(), p.clone())])),
    };
    let req = |key, arguments| Request {
        sandbox_id: "sandbox-test".into(),
        principal_id: "prn_requester".into(),
        execution_id: "exe_test".into(),
        action: "action".into(),
        idempotency_key: key,
        arguments,
    };
    let identity = || Identity {
        sandbox_id: "sandbox-test".into(),
        principal_id: "prn_requester".into(),
    };
    let key = Uuid::new_v4();
    let admitted = repo
        .request(req(key, json!({"frozen":true})), &client)
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(admitted["id"].clone()).unwrap();
    assert_eq!(
        repo.request(req(key, json!({"frozen":true})), &client)
            .await
            .unwrap(),
        admitted
    );
    assert!(
        repo.request(req(key, json!({"frozen":false})), &client)
            .await
            .is_err()
    );
    assert!(
        repo.read(
            id,
            Identity {
                sandbox_id: "other".into(),
                principal_id: "prn_requester".into()
            }
        )
        .await
        .is_err()
    );
    let task: String =
        sqlx::query_scalar("select workflow_task_id::text from tool_approvals where id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(repo.load(id, &Uuid::new_v4().to_string()).await.is_err());
    let row = repo.load(id, &task).await.unwrap();
    let params: Value =
        sqlx::query_scalar("select params from absurd.t_centaur_workflows where task_id=$1::uuid")
            .bind(&task)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(params["input"]["approval_id"], json!(id));
    let blocks = json!([{"type":"section","text":{"type":"plain_text","text":row.payload_json}}]);
    sqlx::query("update tool_approvals set message_ts='2.000',message_blocks=$2 where id=$1")
        .bind(id)
        .bind(&blocks)
        .execute(&pool)
        .await
        .unwrap();
    let button = |user: &str, action: &str| slack_buttons::VerifiedButton {
        request: CreateWorkflowRunRequest {
            workflow_name: DECISION_WORKFLOW.into(),
            input: json!({"approval_id":id,"payload_hash":row.payload_hash,"click":{"id":id,"action":action,"channel_id":"C1","message_ts":"2.000","team_id":"T1","user_id":user}}),
            idempotency_key: Some("test".into()),
            harness_type: None,
            max_attempts: None,
        },
        message: Some(json!({"blocks":blocks})),
    };
    for (user, reason) in [
        ("U2", DecisionRejection::SelfApprovalNotAllowed),
        ("UUNKNOWN", DecisionRejection::NotApprover),
    ] {
        for decision in ["approved", "declined"] {
            assert!(matches!(repo.decide(button(user, decision)).await,
                Err(WorkflowRuntimeError::ApprovalDecisionRejected(actual)) if actual == reason));
            assert_eq!(
                repo.read(id, identity()).await.unwrap()["status"],
                "pending"
            );
        }
    }
    for field in ["id", "channel_id", "message_ts", "team_id"] {
        let mut forged = button("U1", "approved");
        forged.request.input["click"][field] = json!("OTHER");
        assert!(matches!(
            repo.decide(forged).await,
            Err(WorkflowRuntimeError::Disabled(_))
        ));
    }
    let mut changed_policy = p.clone();
    changed_policy.implementation_revision = "new-reviewed-commit".into();
    let changed_repo = Repository {
        pool: pool.clone(),
        policies: Arc::new(BTreeMap::from([("action".into(), changed_policy)])),
    };
    assert!(matches!(
        changed_repo.decide(button("U1", "approved")).await,
        Err(WorkflowRuntimeError::ApprovalDecisionRejected(
            DecisionRejection::PolicyChanged
        ))
    ));
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "pending"
    );
    let mut forged = button("U1", "approved");
    forged.message.as_mut().unwrap()["blocks"][0]["text"]["text"] = json!("changed");
    assert!(matches!(
        repo.decide(forged).await,
        Err(WorkflowRuntimeError::Disabled(_))
    ));
    // Requests survive the agent turn ending.
    sqlx::query("update session_executions set status='completed' where execution_id='exe_test'")
        .execute(&pool)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        repo.decide(button("U1", "approved")),
        repo.decide(button("U3", "declined"))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    if repo.read(id, identity()).await.unwrap()["status"] == "declined" {
        assert!(repo.claim(id, &p).await.unwrap().is_none());
        // Independent test setup for the execution claim race.
        sqlx::query("update tool_approvals set status='approved' where id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (a, b) = tokio::join!(repo.claim(id, &p), repo.claim(id, &p));
    assert_eq!(
        usize::from(a.unwrap().is_some()) + usize::from(b.unwrap().is_some()),
        1
    );
    repo.recover(id).await.unwrap();
    assert!(matches!(
        repo.decide(button("U1", "approved")).await,
        Err(WorkflowRuntimeError::ApprovalDecisionRejected(
            DecisionRejection::Unknown
        ))
    ));
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "unknown"
    );
    assert!(repo.claim(id, &p).await.unwrap().is_none());
    // Public output is persisted only after the one-shot execution claim.
    let public_policy = p.clone();
    let hello = || Ok(json!({"message":"Hello world!","credential":"do-not-publish"}));
    repo.finish(id, &public_policy, hello()).await.unwrap();
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "unknown"
    );
    // Independent setup to exercise completion without rerunning an executor.
    sqlx::query("update tool_approvals set status='executing' where id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let mut wrong_policy = public_policy.clone();
    wrong_policy.public_result_fields.push("credential".into());
    repo.finish(id, &wrong_policy, hello()).await.unwrap();
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "executing"
    );
    repo.finish(id, &public_policy, hello()).await.unwrap();
    let completed = repo.read(id, identity()).await.unwrap();
    assert_eq!(completed["status"], "succeeded");
    assert_eq!(
        completed["result"],
        json!({"outcome":"succeeded","output":{"message":"Hello world!"}})
    );
    let loaded = repo.load(id, &task).await.unwrap();
    assert_eq!(loaded.result.as_ref(), Some(&completed["result"]));
    let message = runner::final_message(
        id,
        &loaded.action,
        &loaded.status,
        loaded.result.as_ref(),
        Some(&public_policy),
    );
    assert!(message["text"].as_str().unwrap().contains("Hello world!"));
    assert!(!message.to_string().contains("do-not-publish"));
    assert!(repo.claim(id, &p).await.unwrap().is_none());
    repo.finish(id, &public_policy, Err(unavailable()))
        .await
        .unwrap();
    assert_eq!(repo.read(id, identity()).await.unwrap(), completed);
    for (expected, expired, revoked) in [
        ("cancelled", false, true),
        ("expired", true, false),
        ("cancelled", false, false),
    ] {
        sqlx::query("update tool_approvals set status='pending',expires_at=now()+interval '1 hour' where id=$1").bind(id).execute(&pool).await.unwrap();
        if expired {
            sqlx::query(
                "update tool_approvals set expires_at=now()-interval '1 second' where id=$1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        if !expired && !revoked {
            repo.cancel(id, identity()).await.unwrap();
        }
        repo.refresh(id, if revoked { None } else { Some(&p) })
            .await
            .unwrap();
        assert_eq!(repo.read(id, identity()).await.unwrap()["status"], expected);
        let reason = if expired {
            DecisionRejection::Expired
        } else {
            DecisionRejection::Cancelled
        };
        assert!(matches!(repo.decide(button("U1", "approved")).await,
            Err(WorkflowRuntimeError::ApprovalDecisionRejected(actual)) if actual == reason));
    }
    // Repair uses durable native terminal state, including when the worker
    // never ran again. Known results are preserved and no authority is restored.
    for native in ["cancelled", "failed", "completed"] {
        sqlx::query("update absurd.t_centaur_workflows set state=$2 where task_id=$1::uuid")
            .bind(&task)
            .bind(native)
            .execute(&pool)
            .await
            .unwrap();
        for (state, expected) in [
            ("pending", "cancelled"),
            ("approved", "cancelled"),
            ("executing", "unknown"),
            ("succeeded", "succeeded"),
            ("declined", "declined"),
        ] {
            sqlx::query("update tool_approvals set status=$2,expires_at=now()+interval '1 hour',execution_deadline=now()+interval '1 hour' where id=$1")
                .bind(id).bind(state).execute(&pool).await.unwrap();
            repository::reconcile_lifecycle(&pool, None).await.unwrap();
            assert_eq!(repo.read(id, identity()).await.unwrap()["status"], expected);
            assert!(repo.claim(id, &p).await.unwrap().is_none());
        }
    }
    sqlx::query("update absurd.t_centaur_workflows set state='pending' where task_id=$1::uuid")
        .bind(&task)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update tool_approvals set status='executing',execution_deadline=now()-interval '1 second' where id=$1")
        .bind(id).execute(&pool).await.unwrap();
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "unknown"
    );
    repo.finish(id, &p, hello()).await.unwrap();
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "unknown"
    );
    // Late completion must be uncertain even if reconciliation has not run yet.
    sqlx::query("update tool_approvals set status='executing',execution_deadline=clock_timestamp()-interval '1 second' where id=$1")
        .bind(id).execute(&pool).await.unwrap();
    repo.finish(id, &p, hello()).await.unwrap();
    let late = repo.load(id, &task).await.unwrap();
    assert_eq!(late.status, "unknown");
    assert_eq!(late.result, Some(json!({"outcome":"unknown"})));

    // A delayed claim commit must consume the database budget, not reset it.
    let mut short_policy = p.clone();
    short_policy.timeout_seconds = 1;
    sqlx::query("update tool_approvals set status='approved',policy_hash=$2 where id=$1")
        .bind(id)
        .bind(hash(&short_policy))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("create function delay_approval_claim_commit() returns trigger language plpgsql as $$ begin perform pg_sleep(1.05); return new; end $$;
        create constraint trigger delayed_approval_claim after update on tool_approvals deferrable initially deferred for each row when (old.status='approved' and new.status='executing') execute function delay_approval_claim_commit();")
        .execute(&pool).await.unwrap();
    let delayed_deadline = repo.claim(id, &short_policy).await.unwrap().unwrap();
    assert!(delayed_deadline.check().is_err());
    sqlx::raw_sql("drop trigger delayed_approval_claim on tool_approvals; drop function delay_approval_claim_commit();")
        .execute(&pool).await.unwrap();
    repo.finish(id, &short_policy, hello()).await.unwrap();
    assert_eq!(repo.load(id, &task).await.unwrap().status, "unknown");
    sqlx::query("update tool_approvals set policy_hash=$2 where id=$1")
        .bind(id)
        .bind(hash(&p))
        .execute(&pool)
        .await
        .unwrap();

    // Persist before slow shutdown, and stop waiting at the original deadline.
    sqlx::query("update tool_approvals set status='executing',execution_deadline=clock_timestamp()+interval '1 hour' where id=$1")
        .bind(id).execute(&pool).await.unwrap();
    let cleanup_started = std::sync::atomic::AtomicBool::new(false);
    let deadline =
        crate::approvals::execution::Deadline::from_remaining(tokio::time::Instant::now(), 0.2);
    let claim = crate::approvals::execution::ClaimedExecution {
        repo: &repo,
        id,
        policy: &p,
        deadline,
    };
    tokio::time::timeout(Duration::from_secs(2), claim.complete(hello(), async {
        assert_eq!(repo.load(id, &task).await.unwrap().status, "succeeded");
        cleanup_started.store(true, std::sync::atomic::Ordering::SeqCst);
        sqlx::query("update tool_approvals set execution_deadline=clock_timestamp()-interval '1 second' where id=$1")
            .bind(id).execute(&pool).await.unwrap();
        repository::reconcile_lifecycle(&pool, Some(id)).await.unwrap();
        assert_eq!(repo.load(id, &task).await.unwrap().status, "succeeded");
        std::future::pending::<()>().await;
    })).await.unwrap().unwrap();
    assert!(cleanup_started.load(std::sync::atomic::Ordering::SeqCst));
    assert!(deadline.check().is_err());
    assert_eq!(repo.load(id, &task).await.unwrap().status, "succeeded");

    // Race the native cancellation against the one-use execution claim.
    sqlx::query("update tool_approvals set status='approved' where id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (claim, cancelled) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(repo.claim(id, &p), client.cancel_task(&task, None))
    })
    .await
    .unwrap();
    cancelled.unwrap();
    let expected = if claim.unwrap().is_some() {
        "unknown"
    } else {
        "cancelled"
    };
    assert_eq!(repo.read(id, identity()).await.unwrap()["status"], expected);
    assert!(repo.decide(button("U1", "approved")).await.is_err());
    assert!(repo.claim(id, &p).await.unwrap().is_none());

    // Opting in to self-approval still checks the approver allowlist, binds the
    // complete signed/preformatted card, and never allows a second decision.
    sqlx::query("update session_executions set status='running' where execution_id='exe_test'")
        .execute(&pool)
        .await
        .unwrap();
    let mut self_policy = p.clone();
    self_policy.allow_self_approval = true;
    let self_repo = Repository {
        pool: pool.clone(),
        policies: Arc::new(BTreeMap::from([("action".into(), self_policy.clone())])),
    };
    for decision in ["approved", "declined"] {
        let admitted = self_repo
            .request(req(Uuid::new_v4(), json!({})), &client)
            .await
            .unwrap();
        let self_id: Uuid = serde_json::from_value(admitted["id"].clone()).unwrap();
        let self_row: repository::Record =
            sqlx::query_as("select * from tool_approvals where id=$1")
                .bind(self_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut message = runner::card(&self_row);
        slack_buttons::sign_message(&mut message, b"test-secret").unwrap();
        sqlx::query("update tool_approvals set message_ts='3.000',message_blocks=$2 where id=$1")
            .bind(self_id)
            .bind(&message["blocks"])
            .execute(&pool)
            .await
            .unwrap();
        let click = |user: &str, action: &str| {
            let buttons = message["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "actions")
                .unwrap();
            let button = &buttons["elements"][usize::from(action == "declined")];
            let mut echoed = message.clone();
            // Slack adds generated block IDs; visible contents remain bound.
            for (index, block) in echoed["blocks"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                block["block_id"] = json!(format!("slack-{index}"));
            }
            slack_buttons::verify_button(slack_buttons::Invocation {
                button: button["value"].as_str().unwrap().into(),
                click: json!({"id":self_id,"action":action,"channel_id":"C1","message_ts":"3.000",
                    "team_id":"T1","user_id":user,"action_ts":"4.000"}),
                idempotency_key: "self-approval-click".into(),
                message: Some(echoed),
            }, b"test-secret").unwrap()
        };
        assert!(matches!(
            self_repo.decide(click("UUNKNOWN", decision)).await,
            Err(WorkflowRuntimeError::ApprovalDecisionRejected(
                DecisionRejection::NotApprover
            ))
        ));
        let accepted = self_repo.decide(click("U2", decision)).await.unwrap();
        assert!(accepted.created);
        assert_eq!(accepted.status, decision);
        assert!(
            !self_repo
                .decide(click("U2", decision))
                .await
                .unwrap()
                .created
        );
        let opposite = if decision == "approved" {
            "declined"
        } else {
            "approved"
        };
        let expected = if decision == "approved" {
            DecisionRejection::AlreadyApproved
        } else {
            DecisionRejection::AlreadyDeclined
        };
        assert!(matches!(self_repo.decide(click("U2", opposite)).await,
            Err(WorkflowRuntimeError::ApprovalDecisionRejected(reason)) if reason == expected));
        let saved = self_repo.read(self_id, identity()).await.unwrap();
        assert_eq!(saved["status"], decision);
        assert_eq!(saved["decided_by"], "U2");
        if decision == "approved" {
            assert!(
                self_repo
                    .claim(self_id, &self_policy)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                self_repo
                    .claim(self_id, &self_policy)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                self_repo.decide(click("U2", "approved")).await,
                Err(WorkflowRuntimeError::ApprovalDecisionRejected(
                    DecisionRejection::Executing
                ))
            ));
            self_repo
                .finish(self_id, &self_policy, hello())
                .await
                .unwrap();
            assert!(matches!(
                self_repo.decide(click("U2", "approved")).await,
                Err(WorkflowRuntimeError::ApprovalDecisionRejected(
                    DecisionRejection::Succeeded
                ))
            ));
        } else {
            assert!(
                self_repo
                    .claim(self_id, &self_policy)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
    pool.close().await;
    sqlx::query(&format!("drop schema {schema} cascade"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
