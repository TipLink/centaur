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
    p.approver_user_ids.push("U3".into());
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
    for user in ["U2", "UUNKNOWN"] {
        assert!(repo.decide(button(user, "approved")).await.is_err());
    }
    for field in ["id", "channel_id", "message_ts", "team_id"] {
        let mut forged = button("U1", "approved");
        forged.request.input["click"][field] = json!("OTHER");
        assert!(repo.decide(forged).await.is_err());
    }
    let mut forged = button("U1", "approved");
    forged.message.as_mut().unwrap()["blocks"][0]["text"]["text"] = json!("changed");
    assert!(repo.decide(forged).await.is_err());
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
        assert!(!repo.claim(id, &p).await.unwrap());
        // Independent test setup for the execution claim race.
        sqlx::query("update tool_approvals set status='approved' where id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (a, b) = tokio::join!(repo.claim(id, &p), repo.claim(id, &p));
    assert_eq!(usize::from(a.unwrap()) + usize::from(b.unwrap()), 1);
    repo.recover(id).await.unwrap();
    assert_eq!(
        repo.read(id, identity()).await.unwrap()["status"],
        "unknown"
    );
    assert!(!repo.claim(id, &p).await.unwrap());
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
        &loaded.status,
        loaded.result.as_ref(),
        Some(&public_policy),
    );
    assert!(message["text"].as_str().unwrap().contains("Hello world!"));
    assert!(!message.to_string().contains("do-not-publish"));
    assert!(!repo.claim(id, &p).await.unwrap());
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
        assert!(repo.decide(button("U1", "approved")).await.is_err());
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
            assert!(!repo.claim(id, &p).await.unwrap());
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
    let expected = if claim.unwrap() {
        "unknown"
    } else {
        "cancelled"
    };
    assert_eq!(repo.read(id, identity()).await.unwrap()["status"], expected);
    assert!(repo.decide(button("U1", "approved")).await.is_err());
    assert!(!repo.claim(id, &p).await.unwrap());
    pool.close().await;
    sqlx::query(&format!("drop schema {schema} cascade"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
