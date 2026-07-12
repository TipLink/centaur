use std::process::Command;

const PAUSE_ENV: &str = "CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS";
const CONTROL_KEY_ENV: &str = "CENTAUR_CONTROL_API_KEY";
const OTHER_SERVICE_KEY_ENVS: [&str; 7] = [
    "SLACKBOT_API_KEY",
    "GITHUBBOT_API_KEY",
    "LINEARBOT_API_KEY",
    "DISCORDBOT_API_KEY",
    "TEAMSBOT_API_KEY",
    "WORKFLOW_API_KEY",
    "SLACK_FEEDBACK_API_KEY",
];

#[test]
fn rollback_bridge_binary_refuses_startup_without_explicit_workflow_pause() {
    for value in [None, Some("false"), Some("1")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_centaur-api-server"));
        command.env_remove(PAUSE_ENV);
        if let Some(value) = value {
            command.env(PAUSE_ENV, value);
        }

        let output = command.output().expect("run rollback bridge binary");
        assert!(
            !output.status.success(),
            "rollback bridge unexpectedly started with {PAUSE_ENV}={value:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "rollback bridge refuses to start unless {PAUSE_ENV}=true"
            )),
            "unexpected startup error for {PAUSE_ENV}={value:?}: {stderr}"
        );
    }
}

#[test]
fn rollback_bridge_binary_refuses_missing_or_reused_control_key() {
    let mut missing = Command::new(env!("CARGO_BIN_EXE_centaur-api-server"));
    missing.env(PAUSE_ENV, "true").env_remove(CONTROL_KEY_ENV);
    for name in OTHER_SERVICE_KEY_ENVS {
        missing.env_remove(name);
    }
    let output = missing.output().expect("run rollback bridge binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CENTAUR_CONTROL_API_KEY is configured"),
        "unexpected missing-control startup error: {stderr}"
    );

    for reused_name in OTHER_SERVICE_KEY_ENVS {
        let mut command = Command::new(env!("CARGO_BIN_EXE_centaur-api-server"));
        command
            .env(PAUSE_ENV, "true")
            .env(CONTROL_KEY_ENV, "shared-key");
        for name in OTHER_SERVICE_KEY_ENVS {
            command.env_remove(name);
        }
        command.env(reused_name, "shared-key");

        let output = command.output().expect("run rollback bridge binary");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("must contain distinct service credentials")
                && stderr.contains(CONTROL_KEY_ENV)
                && stderr.contains(reused_name),
            "unexpected startup error for duplicate {reused_name}: {stderr}"
        );
        assert!(!stderr.contains("shared-key"), "secret leaked: {stderr}");
    }
}

#[test]
fn rollback_bridge_binary_refuses_migration_ownership_before_database_access() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_centaur-api-server"));
    command
        .env(PAUSE_ENV, "true")
        .env(CONTROL_KEY_ENV, "control-key")
        .env("RUN_MIGRATIONS", "true")
        .env(
            "DATABASE_URL",
            "postgresql://must-not-connect.invalid/rollback_bridge",
        );
    for name in OTHER_SERVICE_KEY_ENVS {
        command.env_remove(name);
    }
    let output = command.output().expect("run rollback bridge binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "rollback bridge refuses to start with RUN_MIGRATIONS=true; the reviewed forward runtime owns schema migration"
        ),
        "unexpected migration-ownership startup error: {stderr}"
    );
    assert!(
        !stderr.contains("must-not-connect.invalid"),
        "bridge attempted to report database access before rejecting migrations: {stderr}"
    );
}
