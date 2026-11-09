#[path = "support/evolution.rs"]
mod fixture;
use fixture::Fixture;

fn directory_link(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let result = std::process::Command::new("cmd.exe")
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

fn write_observations(path: &std::path::Path) {
    let records: String = (0..3)
        .map(|index| {
            serde_json::json!({
        "timestamp":"2026-11-09T00:00:00Z", "tool":"ExternalSyntheticTool", "tool_category":"other",
        "action": format!("outside-{index}"), "result":"success", "score":1.0
    }).to_string() + "\n"
        })
        .collect();
    fixture::write(path, &records);
}

#[test]
fn issue_252_ordinary_reflect_does_not_read_or_delete_escaping_obs() {
    let f = Fixture::new("");
    f.initialize();
    let outside = f.home.join("outside");
    let source = outside.join("session_boundary.jsonl");
    write_observations(&source);
    directory_link(&outside, &f.project.join("obs"));
    let reflected = f.call(&["reflect"], serde_json::json!({"hook_event_name":"Stop"}));
    assert!(
        reflected.status.success(),
        "Ordinary hooks must remain a no-op on unsafe paths"
    );
    assert!(
        source.exists(),
        "Reflection deleted a fallback file outside its project"
    );
    assert!(
        !f.project.join("metrics.json").exists(),
        "Outside data must not reach analysis"
    );
    f.sql("CREATE TEMP TABLE boundary_assertion(value INTEGER CHECK(value=0)); INSERT INTO boundary_assertion SELECT COUNT(*) FROM observations; DROP TABLE boundary_assertion;");
}

#[test]
fn issue_252_ordinary_reflect_does_not_write_through_project_link() {
    let f = Fixture::new("");
    let outside = f.home.join("outside-project");
    write_observations(&outside.join("obs/session_boundary.jsonl"));
    std::fs::remove_dir(&f.project).unwrap();
    directory_link(&outside, &f.project);
    let reflected = f.call(&["reflect"], serde_json::json!({"hook_event_name":"Stop"}));
    assert!(reflected.status.success());
    assert!(
        !outside.join(".reflect.lock").exists(),
        "Project containment must precede lock/state writes"
    );
    assert!(outside.join("obs/session_boundary.jsonl").exists());
}

#[cfg(unix)]
#[test]
fn issue_252_ordinary_reflect_skips_escaping_individual_fallback() {
    let f = Fixture::new("");
    f.initialize();
    let outside = f.home.join("outside.jsonl");
    write_observations(&outside);
    std::fs::create_dir(f.project.join("obs")).unwrap();
    std::os::unix::fs::symlink(&outside, f.project.join("obs/session_link.jsonl")).unwrap();
    f.call(&["reflect"], serde_json::json!({"hook_event_name":"Stop"}));
    assert!(outside.exists());
    assert!(!f.project.join("metrics.json").exists());
    f.sql("CREATE TEMP TABLE boundary_assertion(value INTEGER CHECK(value=0)); INSERT INTO boundary_assertion SELECT COUNT(*) FROM observations; DROP TABLE boundary_assertion;");
}
