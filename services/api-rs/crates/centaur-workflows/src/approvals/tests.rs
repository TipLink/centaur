use super::*;
mod execution;
mod postgres;

#[test]
fn registry_resolution_freezes_identity_and_fails_closed() {
    let mut p = policy();
    p.executor_principal.clear();
    let raw = serde_json::to_value(&p).unwrap();
    assert!(raw.get("executor_principal").is_none());
    let policies = parse_policies(&json!({"action": raw}).to_string()).unwrap();
    let mut registry = WorkflowPrincipalAssignments {
        registered: BTreeMap::from([("guarded_action".into(), "prn_executor".into())]),
        required: BTreeSet::from(["guarded_action".into()]),
        approval_required: BTreeSet::from(["guarded_action".into()]),
    };
    let resolved = resolve_registry_policies(&policies, &registry, &WorkflowEnablement::all());
    assert_eq!(resolved["action"].executor_principal, "prn_executor");
    assert_eq!(hash(&resolved["action"]), hash(&policy())); // Existing pins keep their binding.
    registry
        .registered
        .insert("guarded_action".into(), "prn_replacement".into());
    let changed = resolve_registry_policies(&policies, &registry, &WorkflowEnablement::all());
    assert_ne!(hash(&resolved["action"]), hash(&changed["action"]));
    let pinned = BTreeMap::from([("action".into(), policy())]);
    assert!(resolve_registry_policies(&pinned, &registry, &WorkflowEnablement::all()).is_empty());
    assert!(
        resolve_registry_policies(&policies, &registry, &WorkflowEnablement::allowlist(""))
            .is_empty()
    );
    registry
        .registered
        .insert("guarded_action".into(), "prn_requester".into());
    assert!(resolve_registry_policies(&policies, &registry, &WorkflowEnablement::all()).is_empty());
    registry.registered.clear();
    assert!(resolve_registry_policies(&policies, &registry, &WorkflowEnablement::all()).is_empty());
    registry
        .registered
        .insert("guarded_action".into(), "prn_executor".into());
    registry.approval_required.clear();
    assert!(resolve_registry_policies(&policies, &registry, &WorkflowEnablement::all()).is_empty());
}

#[tokio::test]
async fn child_admission_uses_the_same_access_gate_without_enqueuing() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .unwrap();
    let client = Client::from_pool_with_options(pool, ClientOptions::default()).unwrap();
    let policies = Arc::new(BTreeMap::from([("action".into(), policy())]));
    let clients = WorkflowQueueClients {
        approval_policies: policies.clone(),
        workflow_host_sandbox: None,
        standard: client.clone(),
        slack_live: client.clone(),
        etl: client.clone(),
        etl_backfill: client,
    };
    let parent = WorkflowTaskInput {
        workflow_name: "ordinary".into(),
        input: json!({}),
        harness_type: HarnessType::Codex,
        slack_button_feedback: None,
    };
    assert_eq!(
        access::classify("ordinary", &policies, None),
        access::WorkflowAccess::Ordinary
    );
    assert_eq!(
        access::classify("guarded_action", &policies, None),
        access::WorkflowAccess::ApprovalRequired
    );
    assert_eq!(
        access::classify(DRIVER_WORKFLOW, &policies, None),
        access::WorkflowAccess::Internal
    );
    for name in [
        "guarded_action",
        DRIVER_WORKFLOW,
        DECISION_WORKFLOW,
        "centaur_approval_future",
    ] {
        assert!(clients.ensure_public_workflow(name).is_err());
        let result = start_python_child_workflow(
            &json!({"workflow_name":name,"input":{}}),
            &parent,
            &clients,
        )
        .await;
        assert!(matches!(result, Err(WorkflowRuntimeError::Disabled(_))));
    }
}

fn policy() -> ActionPolicy {
    ActionPolicy {
        workflow: "guarded_action".into(),
        executor_principal: "prn_executor".into(),
        requester_principals: vec!["prn_requester".into()],
        team_id: "T1".into(),
        approver_user_ids: vec!["U1".into()],
        allow_self_approval: false,
        expires_seconds: 3600,
        timeout_seconds: 60,
        implementation_revision: "reviewed-commit".into(),
        public_result_fields: vec![],
    }
}

#[test]
fn public_result_fields_are_explicit_bounded_and_policy_bound() {
    let mut p = policy();
    let original = serde_json::to_value(&p).unwrap();
    assert!(original.get("public_result_fields").is_none());
    assert!(
        parse_policies(&json!({"action":original}).to_string()).unwrap()["action"]
            .public_result_fields
            .is_empty()
    );
    let old_policy_hash = hash(&p);
    let old_payload = payload(&p, json!({})).unwrap().0;
    p.public_result_fields = vec!["message".into()];
    assert!(parse_policies(&json!({"action":p}).to_string()).is_ok());
    let (new_payload, display) = payload(&p, json!({})).unwrap();
    assert_eq!(new_payload["public_result_fields"], json!(["message"]));
    assert!(display.contains("public_result_fields"));
    assert_ne!(hash(&p), old_policy_hash);
    assert_ne!(hash(&new_payload), hash(&old_payload));
    for fields in [
        vec!["".into()],
        vec!["*".into()],
        vec!["nested.message".into()],
        vec!["message".into(), "message".into()],
        vec!["x".repeat(81)],
        (0..9).map(|i| format!("field{i}")).collect(),
    ] {
        p.public_result_fields = fields;
        assert!(parse_policies(&json!({"action":p}).to_string()).is_err());
    }
}

#[test]
fn executor_output_is_private_by_default_and_errors_are_always_private() {
    let mut p = policy();
    let (status, result) = runner::executor_outcome(
        &p,
        Ok(json!({"message":"Hello world!","credential":"do-not-publish"})),
    );
    assert_eq!(status, "succeeded");
    assert_eq!(result, json!({"outcome":"succeeded"}));
    p.public_result_fields = vec!["message".into()];
    let (status, result) = runner::executor_outcome(
        &p,
        Err(WorkflowRuntimeError::Upstream("do-not-publish".into())),
    );
    assert_eq!(status, "unknown");
    assert_eq!(result, json!({"outcome":"unknown"}));
}

#[test]
fn hello_world_public_result_reaches_slack_without_unselected_fields() {
    let mut p = policy();
    p.public_result_fields = vec!["message".into()];
    let (status, result) = runner::executor_outcome(
        &p,
        Ok(json!({"message":"Hello world!","credential":"do-not-publish"})),
    );
    assert_eq!(
        result,
        json!({"outcome":"succeeded","output":{"message":"Hello world!"}})
    );
    let message = runner::final_message(Uuid::nil(), status, Some(&result), Some(&p));
    assert!(message["text"].as_str().unwrap().contains("Hello world!"));
    assert_eq!(message["mrkdwn"], false);
    assert_eq!(message["blocks"][1]["text"]["type"], "plain_text");
    assert!(!message.to_string().contains("do-not-publish"));
    for status in [
        "pending",
        "approved",
        "declined",
        "expired",
        "cancelled",
        "unknown",
    ] {
        let message = runner::final_message(Uuid::nil(), status, Some(&result), Some(&p));
        assert!(!message.to_string().contains("Hello world!"));
    }
    for policy in [None, Some(policy())] {
        let message =
            runner::final_message(Uuid::nil(), "succeeded", Some(&result), policy.as_ref());
        assert!(!message.to_string().contains("Hello world!"));
    }
}

#[test]
fn malformed_public_output_is_omitted_without_repeating_successful_execution() {
    let mut p = policy();
    p.public_result_fields = vec!["message".into()];
    for raw in [
        json!(null),
        json!("Hello world!"),
        json!({}),
        json!({"message":42}),
        json!({"message":{"credential":"do-not-publish"}}),
        json!({"message":"x".repeat(2001)}),
        json!({"message":"🦄".repeat(200)}),
    ] {
        let (status, result) = runner::executor_outcome(&p, Ok(raw));
        assert_eq!(status, "succeeded");
        assert_eq!(result, json!({"outcome":"succeeded","output_omitted":true}));
    }
    p.public_result_fields.push("other".into());
    let (_, result) = runner::executor_outcome(
        &p,
        Ok(json!({"message":"x".repeat(1100),"other":"x".repeat(1100)})),
    );
    assert_eq!(result, json!({"outcome":"succeeded","output_omitted":true}));
}

#[test]
fn public_output_is_plaintext_and_escaped_in_slack_fallback() {
    let mut p = policy();
    p.public_result_fields = vec!["message".into()];
    let (_, result) = runner::executor_outcome(&p, Ok(json!({"message":"<@U1> & \u{202e}"})));
    let message = runner::final_message(Uuid::nil(), "succeeded", Some(&result), Some(&p));
    let text = message["text"].as_str().unwrap();
    assert!(text.is_ascii());
    assert!(!text.contains("<@"));
    assert!(text.contains("\\u202e"));
    assert_eq!(message["blocks"][1]["text"]["emoji"], false);
}

#[test]
fn policies_fail_closed_and_are_disabled_by_default() {
    assert!(parse_policies("{}").unwrap().is_empty());
    let mut p = policy();
    assert!(parse_policies(&json!({"action":p}).to_string()).is_ok());
    p.requester_principals.push(p.executor_principal.clone());
    assert!(parse_policies(&json!({"action":p}).to_string()).is_err());
    let mut p = policy();
    p.workflow = DRIVER_WORKFLOW.into();
    assert!(parse_policies(&json!({"action":p}).to_string()).is_err());
    assert!(parse_policies(&json!({"action":{"workflow":"guarded_action"}}).to_string()).is_err());
}

#[test]
fn complete_display_preserves_numbers_and_escapes_invisible_text() {
    let arguments: Value =
        serde_json::from_str(r#"{"number":9007199254740993,"text":"<@U1> & \u202e"}"#).unwrap();
    let (p, display) = payload(&policy(), arguments).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&display).unwrap(), p);
    assert!(display.is_ascii());
    assert!(!display.contains("<@"));
    assert!(display.contains("9007199254740993"));
    assert!(payload(&policy(), json!({"data":"x".repeat(12000)})).is_err());
    assert!(payload(&policy(), json!([])).is_err());
}

#[test]
fn slack_generated_fields_are_the_only_ignored_visible_changes() {
    let expected = json!([{"type":"section","text":{"type":"plain_text","text":"payload"}}]);
    let generated = json!([{"type":"section","block_id":"generated","text":{"type":"plain_text","text":"payload","emoji":true}}]);
    assert_eq!(visible_blocks(&expected), visible_blocks(&generated));
    let mut forged = generated;
    forged[0]["text"]["text"] = json!("other payload");
    assert_ne!(visible_blocks(&expected), visible_blocks(&forged));
}
