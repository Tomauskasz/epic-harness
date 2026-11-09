#[cfg(windows)]
#[test]
fn configured_formatter_resolves_npx_cmd_and_records_child_failures() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for failure in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        let data = temp.path().join("data");
        let tools = temp.path().join("tool bin");
        for directory in [
            &cwd,
            &tools,
            &data.join("projects/project"),
            &home.join(".config/epic-harness"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(home.join(".config/epic-harness/telemetry-consent"), "off").unwrap();
        std::fs::write(
            data.join("config.toml"),
            "[dashboard]\nport=0\nauto_open=false\n",
        )
        .unwrap();
        std::fs::write(cwd.join(".prettierrc"), "{}").unwrap();
        std::fs::write(cwd.join("source with spaces.js"), "const value=1;\n").unwrap();
        let stub = format!(
            "@echo off\r\n>\"%~dp0called.txt\" echo called\r\necho synthetic formatter diagnostic\r\nexit /b {}\r\n",
            if failure { 7 } else { 0 }
        );
        std::fs::write(tools.join("npx.cmd"), stub).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_epic-harness"))
            .arg("polish")
            .current_dir(&cwd)
            .env("HARNESS_DIR", &data)
            .env("HARNESS_ROOT", temp.path())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("EPIC_HOOK_PROFILE", "standard")
            .env(
                "PATH",
                format!(
                    "{};{}\\System32",
                    tools.display(),
                    std::env::var("SystemRoot").unwrap()
                ),
            )
            .env_remove("HARNESS_DB_URL")
            .env_remove("HARNESS_MEM_URL")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(serde_json::json!({"hook_event_name":"PostToolUse","session_id":"synthetic-formatter","tool_name":"Edit","tool_input":{"file_path":"source with spaces.js"}}).to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "ordinary polish remains a nonblocking hook"
        );
        assert!(
            tools.join("called.txt").is_file(),
            "configured npx.cmd formatter was skipped"
        );
        if failure {
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("synthetic formatter diagnostic"),
                "child failure diagnostics must be visible"
            );
            let stored = Command::new(env!("CARGO_BIN_EXE_epic-harness"))
                .args(["reflect", "--context", "--project", "project"])
                .current_dir(&cwd)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("HARNESS_DIR", &data)
                .env("HARNESS_ROOT", temp.path())
                .env_remove("HARNESS_DB_URL")
                .env_remove("HARNESS_MEM_URL")
                .env_remove("DATABASE_URL")
                .output()
                .unwrap();
            assert!(stored.status.success());
            let json: serde_json::Value = serde_json::from_slice(&stored.stdout).unwrap();
            assert_eq!(json["obs_stats"]["failure_categories"]["lint_fail"], 1);
        }
    }
}
