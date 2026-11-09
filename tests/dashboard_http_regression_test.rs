use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Dashboard {
    _temp: tempfile::TempDir,
    data: PathBuf,
    child: Child,
    port: u16,
}

impl Dashboard {
    fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project-a");
        for dir in [
            &data,
            &cwd,
            &home.join(".config/epic-harness"),
            &data.join("projects/project-a"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(home.join(".config/epic-harness/telemetry-consent"), "off").unwrap();
        fs::write(
            data.join("config.toml"),
            "[dashboard]\nport=0\nauto_open=false\n",
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = Command::new(env!("CARGO_BIN_EXE_epic-harness"))
            .args(["serve", &format!("--port={port}")])
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("HARNESS_ROOT", &root)
            .env("HARNESS_DIR", &data)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env_remove("HARNESS_DB_URL")
            .env_remove("HARNESS_MEM_URL")
            .env_remove("DATABASE_URL")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let dashboard = Self {
            _temp: temp,
            data,
            child,
            port,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fixture dashboard did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        dashboard
    }

    fn request(&self, method: &str, path: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let value = serde_json::from_str(body).unwrap_or_else(|_| json!({"raw": body}));
        (status, value)
    }

    fn file(&self, relative: &str, value: &Value) -> PathBuf {
        let file = self.data.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, serde_json::to_vec(value).unwrap()).unwrap();
        file
    }

    fn seed_pipeline(&self, id: &str, project: &str, state: &str) {
        let _ = self.request("GET", "/api/harness?cmd=get_orbit_pipelines");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            sqlx::any::install_default_drivers();
            let pool = sqlx::AnyPool::connect(&format!(
                "sqlite:{}",
                self.data.join("harness.db").display()
            ))
            .await
            .unwrap();
            epic_harness::store::orbit_store::upsert_pipeline_pool(
                &pool,
                id,
                project,
                "complete",
                Some("evolve"),
                Some("direct"),
                state,
            )
            .await
            .unwrap();
            pool.close().await;
        });
    }

    fn seed_snapshot(&self, project: &str, summary: &str, millis: i64) {
        let _ = self.request("GET", "/api/harness?cmd=get_orbit_pipelines");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            sqlx::any::install_default_drivers();
            let pool = sqlx::AnyPool::connect(&format!(
                "sqlite:{}",
                self.data.join("harness.db").display()
            ))
            .await
            .unwrap();
            epic_harness::store::sessions::insert_snapshot_pool(
                &pool,
                &epic_harness::shared::types::SessionSnapshot {
                    timestamp: format!("2026-11-09T00:00:0{millis}Z"),
                    snap_type: "pre-compact".into(),
                    summary: summary.into(),
                    pending_tasks: vec!["pending synthetic task".into()],
                    context_usage: Some(0.4),
                    pipeline_state: None,
                },
                millis,
                project,
            )
            .await
            .unwrap();
            pool.close().await;
        });
    }

    fn seed_memory(&self, node_type: &str, title: &str) {
        let _ = self.request("GET", "/api/search?q=initialize");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            sqlx::any::install_default_drivers();
            let pool = sqlx::AnyPool::connect(&format!(
                "sqlite:{}",
                self._temp.path().join(".harness/memory.db").display()
            ))
            .await
            .unwrap();
            epic_harness::mem::store::write_node_pool(
                &pool,
                &epic_harness::mem::store::Node {
                    frontmatter: epic_harness::mem::store::NodeFrontmatter {
                        id: uuid::Uuid::new_v4().to_string(),
                        node_type: node_type.into(),
                        title: title.into(),
                        tags: vec!["fixture".into()],
                        projects: vec!["project-a".into()],
                        agents: vec!["synthetic-agent".into()],
                        created: "2026-11-09T00:00:00Z".into(),
                        updated: "2026-11-09T00:00:00Z".into(),
                        importance: 0.8,
                        ..Default::default()
                    },
                    body: "Zebrafixture body".into(),
                },
            )
            .await
            .unwrap();
            pool.close().await;
        });
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn issue_222_nonobject_pipeline_files_do_not_terminate_dashboard() {
    let dashboard = Dashboard::start();
    dashboard.file("projects/project-a/orbit/PIPELINE-invalid.json", &json!([]));
    dashboard.file(
        "projects/project-a/orbit/PIPELINE-valid.json",
        &json!({"id":"valid", "status":"complete"}),
    );
    let (status, body) = dashboard.request("GET", "/api/harness?cmd=get_orbit_pipelines");
    assert_eq!(
        status, 200,
        "non-object sidecar must be skipped, not crash the server"
    );
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], "valid");
    assert_eq!(dashboard.request("GET", "/api/projects").0, 200);
}

#[test]
fn issue_222_nonobject_pipeline_database_rows_do_not_terminate_dashboard() {
    let dashboard = Dashboard::start();
    dashboard.seed_pipeline("invalid", "project-a", "[]");
    dashboard.seed_pipeline("valid", "project-a", "{}");
    let (status, body) = dashboard.request("GET", "/api/harness?cmd=get_orbit_pipelines");
    assert_eq!(status, 200, "non-object database state must be skipped");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], "valid");
    assert_eq!(dashboard.request("GET", "/api/projects").0, 200);
}

#[test]
fn issue_221_empty_pipeline_id_rejects_without_deleting_other_projects() {
    let dashboard = Dashboard::start();
    let a = dashboard.file(
        "projects/project-a/orbit/PIPELINE-alpha.json",
        &json!({"id":"alpha"}),
    );
    let b = dashboard.file(
        "projects/project-b/orbit/PIPELINE-beta.json",
        &json!({"id":"beta"}),
    );
    let (status, body) = dashboard.request("DELETE", "/api/orbit/");
    assert_eq!(status, 400);
    assert_eq!(body["ok"], false);
    assert!(a.exists() && b.exists());
}

#[test]
fn issue_221_pipeline_id_matches_exactly_and_honors_selected_project() {
    let dashboard = Dashboard::start();
    let selected = dashboard.file(
        "projects/project-a/orbit/PIPELINE-alpha.json",
        &json!({"id":"alpha"}),
    );
    let prefix = dashboard.file(
        "projects/project-a/orbit/PIPELINE-alpha-extra.json",
        &json!({"id":"alpha-extra"}),
    );
    let other_project = dashboard.file(
        "projects/project-b/orbit/PIPELINE-alpha.json",
        &json!({"id":"alpha"}),
    );
    let (status, body) = dashboard.request("DELETE", "/api/orbit/alpha?project=project-a");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert!(!selected.exists());
    assert!(prefix.exists() && other_project.exists());
}

#[test]
fn issue_264_dismiss_removes_primary_pipeline_and_sidecar() {
    let dashboard = Dashboard::start();
    dashboard.seed_pipeline("PIPELINE-persisted", "project-a", "{}");
    dashboard.seed_pipeline("PIPELINE-persisted-extra", "project-a", "{}");
    let sidecar = dashboard.file(
        "projects/project-a/orbit/PIPELINE-persisted.json",
        &json!({"id":"PIPELINE-persisted"}),
    );
    let (status, body) =
        dashboard.request("DELETE", "/api/orbit/PIPELINE-persisted?project=project-a");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert!(!sidecar.exists());
    let (_, remaining) = dashboard.request("GET", "/api/harness?cmd=get_orbit_pipelines");
    let rows = remaining.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "successful dismissal must also remove the primary DB row"
    );
    assert_eq!(rows[0]["id"], "PIPELINE-persisted-extra");
}

#[test]
fn issue_264_database_only_pipeline_can_be_dismissed_with_project_scope() {
    let dashboard = Dashboard::start();
    dashboard.seed_pipeline("PIPELINE-db-only", "project-b", "{}");
    let (wrong_status, wrong_body) =
        dashboard.request("DELETE", "/api/orbit/PIPELINE-db-only?project=project-a");
    assert_eq!(wrong_status, 404);
    assert_eq!(wrong_body["ok"], false);
    let (_, before) = dashboard.request("GET", "/api/harness?cmd=get_orbit_pipelines");
    assert_eq!(before.as_array().unwrap().len(), 1);
    let (status, body) =
        dashboard.request("DELETE", "/api/orbit/PIPELINE-db-only?project=project-b");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(
        dashboard
            .request("GET", "/api/harness?cmd=get_orbit_pipelines")
            .1,
        json!([])
    );
}

fn encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn directory_link(link: &std::path::Path, target: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let result = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(target.to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn issue_220_project_selectors_reject_path_syntax_without_read_or_write() {
    let dashboard = Dashboard::start();
    let outside = dashboard.data.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("manifests.jsonl"),
        "{\"marker\":\"OUTSIDE_SENTINEL\"}\n",
    )
    .unwrap();
    let registry = "{\"solved\":{\"session\":1,\"real_task\":0.8},\"total_solved\":2}";
    fs::write(outside.join("seesaw.json"), registry).unwrap();
    for project in [
        "../../outside".to_string(),
        "..\\..\\outside".to_string(),
        outside.to_string_lossy().into_owned(),
    ] {
        for cmd in [
            "get_manifests",
            "get_seesaw_registry",
            "get_variant_pool",
            "get_harness_snapshot",
        ] {
            let (status, body) = dashboard.request(
                "GET",
                &format!("/api/harness?cmd={cmd}&project={}", encoded(&project)),
            );
            assert_eq!(status, 400, "{cmd} must reject selector {project}");
            assert!(body.get("error").is_some());
            assert!(!body.to_string().contains("OUTSIDE_SENTINEL"));
        }
    }
    assert_eq!(
        fs::read_to_string(outside.join("seesaw.json")).unwrap(),
        registry
    );
    let manifest = dashboard.data.join("projects/project-a/manifests.jsonl");
    fs::write(manifest, "{\"marker\":\"INSIDE_CONTROL\"}\n").unwrap();
    let (status, body) =
        dashboard.request("GET", "/api/harness?cmd=get_manifests&project=project-a");
    assert_eq!(status, 200);
    assert_eq!(body[0]["marker"], "INSIDE_CONTROL");
}

#[test]
fn issue_220_seesaw_get_is_read_only() {
    let dashboard = Dashboard::start();
    let file = dashboard.file(
        "projects/project-a/seesaw.json",
        &json!({"solved":{"session":1,"real_task":0.8},"total_solved":2}),
    );
    let before = fs::read(&file).unwrap();
    let (status, _) = dashboard.request(
        "GET",
        "/api/harness?cmd=get_seesaw_registry&project=project-a",
    );
    assert_eq!(status, 200);
    assert_eq!(
        fs::read(file).unwrap(),
        before,
        "a dashboard GET must not migrate or rewrite its source"
    );
}

#[test]
fn issue_220_project_and_nested_junctions_cannot_escape_project_data() {
    let dashboard = Dashboard::start();
    let outside = dashboard.data.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("manifests.jsonl"),
        "{\"marker\":\"OUTSIDE_SENTINEL\"}\n",
    )
    .unwrap();
    directory_link(&dashboard.data.join("projects/linked"), &outside);
    assert_eq!(
        dashboard
            .request("GET", "/api/harness?cmd=get_manifests&project=linked")
            .0,
        400
    );

    for (folder, cmd) in [
        ("evolved", "get_evolved_skills"),
        ("obs", "get_obs_summary"),
        ("orbit", "get_orbit_pipelines"),
        ("sessions", "get_session_snapshots"),
    ] {
        let fixture = Dashboard::start();
        let external = fixture.data.parent().unwrap().join("outside");
        fs::create_dir_all(&external).unwrap();
        directory_link(
            &fixture.data.join("projects/project-a").join(folder),
            &external,
        );
        assert_eq!(
            fixture
                .request("GET", &format!("/api/harness?cmd={cmd}&project=project-a"))
                .0,
            400,
            "{folder} descendant must remain contained"
        );
    }
}

#[test]
fn issue_214_snapshot_api_returns_scoped_database_snapshot_objects() {
    let dashboard = Dashboard::start();
    dashboard.seed_snapshot("project-a", "local snapshot", 1);
    dashboard.seed_snapshot("project-b", "foreign snapshot", 2);
    let (status, body) = dashboard.request(
        "GET",
        "/api/harness?cmd=get_session_snapshots&project=project-a",
    );
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["summary"], "local snapshot");
    assert_eq!(body[0]["type"], "pre-compact");
    assert_eq!(body[0]["context_usage"], 0.4);
    assert_eq!(body[0]["pending_tasks"][0], "pending synthetic task");
    assert!(body[0]["timestamp"].is_string());
    let (_, all) = dashboard.request("GET", "/api/harness?cmd=get_session_snapshots");
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[test]
fn issue_214_snapshot_api_parses_legacy_files_instead_of_returning_filenames() {
    let dashboard = Dashboard::start();
    dashboard.file("projects/project-a/sessions/snapshot-old.json", &json!({"timestamp":"2026-11-08T00:00:00Z","type":"pre-compact","summary":"legacy local","pending_tasks":[],"context_usage":0.2}));
    dashboard.file("projects/project-b/sessions/snapshot-other.json", &json!({"timestamp":"2026-11-09T00:00:00Z","type":"pre-compact","summary":"legacy foreign","pending_tasks":[],"context_usage":0.3}));
    let (_, body) = dashboard.request(
        "GET",
        "/api/harness?cmd=get_session_snapshots&project=project-a",
    );
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["summary"], "legacy local");
    assert_eq!(body[0]["type"], "pre-compact");
    assert!(body[0]["timestamp"].is_string());
    assert_eq!(
        dashboard
            .request("GET", "/api/harness?cmd=get_session_snapshots")
            .1
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn issue_214_duplicate_snapshot_copies_preserve_distinct_projects() {
    let dashboard = Dashboard::start();
    dashboard.seed_snapshot("project-a", "same summary", 1);
    let snapshot = json!({"timestamp":"2026-11-09T00:00:01Z","type":"pre-compact","summary":"same summary","pending_tasks":["pending synthetic task"],"context_usage":0.4});
    dashboard.file("projects/project-a/sessions/copied.json", &snapshot);
    dashboard.file("projects/project-b/sessions/other-project.json", &snapshot);
    let (_, body) = dashboard.request("GET", "/api/harness?cmd=get_session_snapshots");
    let rows = body.as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "deduplicate the same project's file/DB copy, never another project's identical snapshot"
    );
    let projects: std::collections::HashSet<_> = rows
        .iter()
        .filter_map(|row| row["project"].as_str())
        .collect();
    assert_eq!(
        projects,
        std::collections::HashSet::from(["project-a", "project-b"])
    );
}

#[test]
fn issue_240_memory_search_rows_include_renderable_metadata() {
    let dashboard = Dashboard::start();
    dashboard.seed_memory("concept", "Zebrafixture needle");
    let (status, body) = dashboard.request("GET", "/api/search?q=Zebrafixture");
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["tags"], json!(["fixture"]));
    assert_eq!(body[0]["projects"], json!(["project-a"]));
    assert_eq!(body[0]["agents"], json!(["synthetic-agent"]));
    assert!(body[0]["updated"].is_string());
    assert_eq!(body[0]["importance"], 0.8);
    assert!(body[0]["snippet"].is_string());
}

#[test]
fn issue_268_memory_search_filters_type_before_limit() {
    let dashboard = Dashboard::start();
    for index in 0..25 {
        dashboard.seed_memory("concept", &format!("Zebrafixture concept {index}"));
    }
    dashboard.seed_memory("decision", "Zebrafixture decision");
    let (status, body) = dashboard.request("GET", "/api/search?type=decision&q=Zebrafixture");
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["type"], "decision");
    let (_, absent) = dashboard.request("GET", "/api/search?q=Zebrafixture&type=resolution");
    assert_eq!(absent, json!([]));
    let (_, all) = dashboard.request("GET", "/api/search?q=Zebrafixture");
    assert_eq!(all.as_array().unwrap().len(), 20);
}

#[test]
fn issue_268_memory_search_rejects_unknown_type() {
    let dashboard = Dashboard::start();
    let (status, body) = dashboard.request("GET", "/api/search?q=Zebrafixture&type=bogus");
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}
