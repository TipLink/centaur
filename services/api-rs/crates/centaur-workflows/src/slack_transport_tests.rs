use super::*;

// One synchronous test owns these environment keys and restores them on panic.
// No other test invokes the Slack transport or changes its configuration.
#[test]
fn slack_transport_rejects_ungranted_workflows_and_missing_configuration() {
    const KEYS: [&str; 3] = [
        "WORKFLOW_SLACK_TRANSPORT_ALLOWED_NAMES",
        "WORKFLOW_SLACK_TRANSPORT_URL",
        "SLACKBOT_API_KEY",
    ];
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                unsafe {
                    if let Some(value) = value {
                        env::set_var(key, value);
                    } else {
                        env::remove_var(key);
                    }
                }
            }
        }
    }
    let _restore = Restore(KEYS.map(|key| (key, env::var_os(key))).into());
    for key in KEYS {
        unsafe { env::remove_var(key) };
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let input = WorkflowTaskInput {
        workflow_name: "work_item_job".to_owned(),
        input: json!({}),
        harness_type: HarnessType::Codex,
    };
    let request = json!({"operation": "post", "args": {"channel": "C1", "text": "test"}});
    let error = runtime
        .block_on(python_slack_transport(&request, &input))
        .unwrap_err();
    assert!(error.to_string().contains("no Slack transport grant"));

    unsafe {
        env::set_var(
            "WORKFLOW_SLACK_TRANSPORT_ALLOWED_NAMES",
            "work_item_job_extra",
        );
    }
    let error = runtime
        .block_on(python_slack_transport(&request, &input))
        .unwrap_err();
    assert!(error.to_string().contains("no Slack transport grant"));

    unsafe {
        env::set_var(
            "WORKFLOW_SLACK_TRANSPORT_ALLOWED_NAMES",
            "other_workflow, work_item_job ",
        );
    }
    let error = runtime
        .block_on(python_slack_transport(&request, &input))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Slack transport is not configured")
    );

    unsafe {
        env::set_var(
            "WORKFLOW_SLACK_TRANSPORT_URL",
            "http://127.0.0.1:1/internal/workflow/slack",
        );
    }
    let error = runtime
        .block_on(python_slack_transport(&request, &input))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Slack transport credential is not configured")
    );
}
