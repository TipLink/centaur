//! Exercise the CLI entry point with an isolated environment and offline connectors.

use std::{
    collections::BTreeSet,
    fs,
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};

const CONNECTORS: [&str; 5] = ["attio", "linear", "mercator", "slack", "websearch"];

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn check(env: &[(&str, &str)], expected: &[&str]) {
    let root = tempfile::tempdir().unwrap();
    for name in CONNECTORS {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("connector.toml"), "description = 'Policy fixture'").unwrap();
        fs::write(dir.join("index.ts"), format!(
            "/** Echo the connector name. */\nexport async function echo(args: {{}}): Promise<string> {{ return '{name}'; }}"
        )).unwrap();
    }
    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_centaur-codemode"));
        cmd.env_clear()
            .envs(env.iter().copied())
            .arg("--connectors")
            .arg(root.path());
        cmd.args(args).output().unwrap()
    };
    let listed = run(&["list"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let list = String::from_utf8(listed.stdout).unwrap();
    assert_eq!(
        list.lines()
            .map(|line| line.split_once(':').unwrap().0)
            .collect::<Vec<_>>(),
        expected
    );

    if let Some(first) = expected.first() {
        let narrowed = run(&["--tools", first, "list"]);
        assert!(narrowed.status.success());
        let list = String::from_utf8(narrowed.stdout).unwrap();
        assert_eq!(list.trim_end(), format!("{first}: Policy fixture"));
    }

    let search = run(&["search", "echo"]);
    assert!(search.status.success());
    let hits: Vec<Value> = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit["tool"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        expected.iter().copied().collect()
    );

    for name in CONNECTORS {
        let allowed = expected.contains(&name);
        let load = run(&["load", name]);
        assert_eq!(load.status.success(), allowed, "load {name}");
        let exec = run(&[
            "--tools",
            name,
            "exec",
            "-e",
            &format!("return await tools.{name}.echo({{}})"),
        ]);
        assert_eq!(
            exec.status.success(),
            allowed,
            "exec {name}: {}",
            String::from_utf8_lossy(&exec.stderr)
        );
        let body: Value = serde_json::from_slice(&exec.stdout).unwrap();
        assert_eq!(body["ok"], allowed);
        if allowed {
            assert_eq!(body["result"], name);
        } else {
            assert_eq!(body["calls"], json!([]));
        }
    }
}

#[test]
fn dedicated_list_replaces_python_packages() {
    check(
        &[
            ("TOOL_ALLOWLIST", "attio,linear,slack"),
            ("CENTAUR_CODEMODE_ALLOWLIST", "websearch,mercator"),
        ],
        &["mercator", "websearch"],
    );
}

#[test]
fn explicitly_empty_dedicated_list_denies_every_connector() {
    for empty in ["", "  \t "] {
        check(
            &[
                ("TOOL_ALLOWLIST", "attio,linear"),
                ("CENTAUR_CODEMODE_ALLOWLIST", empty),
            ],
            &[],
        );
    }
}

#[test]
fn legacy_allowlist_retains_its_empty_and_nonempty_behavior() {
    check(&[], &CONNECTORS);
    check(&[("TOOL_ALLOWLIST", " , ")], &CONNECTORS);
    check(
        &[("TOOL_ALLOWLIST", "attio, linear,")],
        &["attio", "linear"],
    );
}

#[test]
fn both_blocklists_take_precedence() {
    check(
        &[
            ("CENTAUR_CODEMODE_ALLOWLIST", "websearch,mercator,attio"),
            ("TOOL_BLOCKLIST", "attio"),
            ("CENTAUR_CODEMODE_BLOCKLIST", "mercator"),
        ],
        &["websearch"],
    );
}

#[tokio::test]
async fn serve_enforces_dedicated_allowlist_from_environment() {
    let root = tempfile::tempdir().unwrap();
    for name in CONNECTORS {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("connector.toml"), "description = 'Policy fixture'").unwrap();
        fs::write(
            dir.join("index.ts"),
            format!(
                "/** Echo the connector name. */\nexport async function echo(args: {{}}): Promise<string> {{ return '{name}'; }}"
            ),
        )
        .unwrap();
    }
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let stderr_path = root.path().join("serve.stderr");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_centaur-codemode"))
        .env_clear()
        .env("TOOL_ALLOWLIST", "attio,linear,slack")
        .env("CENTAUR_CODEMODE_ALLOWLIST", "websearch,mercator")
        .arg("--connectors")
        .arg(root.path())
        .arg("serve")
        .arg("--listen")
        .arg(addr.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..200 {
        if let Ok(resp) = client.get(format!("{base}/healthz")).send().await
            && resp.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ready,
        "centaur-codemode serve did not become healthy; stderr: {}",
        fs::read_to_string(&stderr_path).unwrap_or_default()
    );

    let catalog: Value = client
        .get(format!("{base}/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed: Vec<&str> = catalog["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["mercator", "websearch"]);

    let denied_load = client
        .post(format!("{base}/load"))
        .json(&json!({"tool": "attio", "policy": {"allowlist": ["attio"]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied_load.status(), reqwest::StatusCode::NOT_FOUND);

    let allowed_load = client
        .post(format!("{base}/load"))
        .json(&json!({"tool": "websearch"}))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_load.status(), reqwest::StatusCode::OK);

    let denied_exec: Value = client
        .post(format!("{base}/exec"))
        .json(&json!({
            "code": "return await tools.attio.echo({})",
            "policy": {"allowlist": ["attio"]}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(denied_exec["ok"], false);
    assert_eq!(denied_exec["calls"], json!([]));
}
