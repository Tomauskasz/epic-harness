use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::paths::{canonical_for_compare, orbit_dir};

/// Scan a directory for PIPELINE-*.json files with `"status": "running"`.
/// Returns the most recent running pipeline (by filename sort order), or None.
/// Returns None immediately if the directory does not exist (hot path optimization).
/// Symlinks are skipped to prevent path traversal attacks.
/// When multiple running files exist (should not happen; concurrent-orbit guard prevents it),
/// logs a warning and returns the most recently named one deterministically.
pub(crate) fn scan_running_pipeline_in(dir: &Path) -> Option<serde_json::Value> {
    if !dir.is_dir() {
        return None;
    }
    // Collect all PIPELINE-*.json entries (non-symlink) sorted by filename for determinism.
    // The PIPELINE-{timestamp} naming makes lexicographic order equal to creation order.
    let mut candidates: Vec<(String, serde_json::Value)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("PIPELINE-") || !name_str.ends_with(".json") {
                return None;
            }
            let path = entry.path();
            // Symlink defense: skip symlinks to prevent path traversal.
            // Cache the metadata to avoid a second stat between the check and the read.
            let meta = path.symlink_metadata().ok()?;
            if meta.file_type().is_symlink() {
                return None;
            }
            let content = fs::read_to_string(&path).ok()?;
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(val) if val.get("status").and_then(|v| v.as_str()) == Some("running") => {
                    Some((name_str.into_owned(), val))
                }
                Err(_) => {
                    eprintln!("[orbit] WARNING: Failed to parse {}", name_str);
                    None
                }
                _ => None,
            }
        })
        .collect();

    if candidates.len() > 1 {
        eprintln!(
            "[orbit] WARNING: {} running pipeline files found; using most recent",
            candidates.len()
        );
    }
    // Sort ascending by filename; the last element is the most recent timestamp.
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.into_iter().last().map(|(_, val)| val)
}

/// Scan the orbit directory for a running pipeline.
/// Delegates to `scan_running_pipeline_in` with the real orbit directory.
fn scan_running_pipeline() -> Option<serde_json::Value> {
    scan_running_pipeline_in(&orbit_dir())
}

// ── Shared orbit state cache ──────────────────────────
// Used by detect_active_orbit_id() (called from observe.rs and polish.rs on every hook fire)
// to avoid a full directory scan per tool call. TTL mirrors the guard.rs cache.

struct OrbitIdCache {
    value: Option<serde_json::Value>,
    cached_at: Instant,
    initialized: bool,
}

static ORBIT_ID_CACHE: OnceLock<Mutex<OrbitIdCache>> = OnceLock::new();
const ORBIT_ID_CACHE_TTL_SECS: u64 = 60;

fn cached_orbit_state_common() -> Option<serde_json::Value> {
    let cache = ORBIT_ID_CACHE.get_or_init(|| {
        Mutex::new(OrbitIdCache {
            value: None,
            cached_at: Instant::now(),
            initialized: false,
        })
    });
    let mut guard = cache.lock().unwrap();
    let expired =
        !guard.initialized || guard.cached_at.elapsed().as_secs() >= ORBIT_ID_CACHE_TTL_SECS;
    if !expired {
        return guard.value.clone();
    }
    let value = scan_running_pipeline();
    guard.value = value.clone();
    guard.cached_at = Instant::now();
    guard.initialized = true;
    value
}

/// Detect an active orbit pipeline by scanning PIPELINE-*.json files.
/// Results are cached for 60 seconds to avoid a directory scan per hook call.
/// Returns Some(pipeline_id) if a file with `"status": "running"` exists.
/// The returned ID is normalized: only `a-z`, `0-9`, `-`, `_` are kept.
pub fn detect_active_orbit_id() -> Option<String> {
    let val = cached_orbit_state_common()?;
    val.get("id")
        .and_then(|v| v.as_str())
        .map(normalize_pipeline_id)
}

/// Read the full pipeline state for an active orbit (uncached, authoritative).
/// Use this when you need the latest state, not a cached snapshot.
pub fn read_active_orbit_state() -> Option<serde_json::Value> {
    scan_running_pipeline()
}

/// Normalize a raw pipeline ID for safe use in filenames and observation records.
/// Keeps only `a-z`, `0-9`, `-`, `_`; replaces all other characters with `-`.
/// Truncates to 128 characters.
pub fn normalize_pipeline_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(128)
        .collect()
}

/// Check the invariants a completed orbit pipeline is supposed to satisfy.
///
/// The pipeline file is written by the orbit skill, so nothing in the harness
/// could previously contradict it: pipelines were observed marked `complete`
/// with no PR or CI proof, and one with `audit_fail_count` above its own
/// `max_retries` — the skill is supposed to pause instead. Consumers must run
/// this validation before persistence so a self-declared status cannot become
/// dashboard evidence.
///
/// Returns one message per violated invariant; empty means the state is
/// self-consistent. Pipelines that are not complete are not checked — an
/// in-flight pipeline is legitimately missing most of this.
pub fn completion_violations(pipeline: &serde_json::Value) -> Vec<String> {
    let status = pipeline.get("status").and_then(|v| v.as_str());
    if !matches!(status, Some("complete") | Some("shipped")) {
        return Vec::new();
    }

    let mut violations = Vec::new();
    let num = |key: &str| pipeline.get(key).and_then(|v| v.as_u64());

    if let (Some(fails), Some(max)) = (num("audit_fail_count"), num("max_retries"))
        && fails > max
    {
        violations.push(format!(
            "completed with audit_fail_count={fails} above max_retries={max}; the run should have paused for a decision"
        ));
    }

    let has_pr = pipeline
        .get("pr_url")
        .and_then(|v| v.as_str())
        .is_some_and(is_github_pull_request_url);
    if !has_pr {
        violations.push("completed without a concrete GitHub pull-request URL".to_string());
    }

    if pipeline.get("ci_status").and_then(|v| v.as_str()) != Some("success") {
        violations.push("completed without ci_status=\"success\" evidence".to_string());
    }

    violations
}

/// Mark the latest running Orbit pipeline complete after validating its evidence.
///
/// This is the only completion persistence path. It accepts a valid legacy
/// completion unchanged, but never changes invalid state. The target must be a
/// regular file below the regular `harness_dir/orbit` directory; valid writes
/// are staged and atomically replaced.
pub fn complete_pipeline_in(harness_dir: &Path) -> io::Result<()> {
    let orbit_dir = regular_orbit_dir(harness_dir)?;
    let (pipeline_path, state) = completion_candidate(&orbit_dir, harness_dir)?;
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Orbit pipeline has no status")
        })?;

    if status == "complete" {
        return ensure_valid_completion(&state);
    }

    let mut completed = state;
    let object = completed.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Orbit pipeline state must be a JSON object",
        )
    })?;
    object.insert(
        "status".into(),
        serde_json::Value::String("complete".into()),
    );
    object.insert("phase".into(), serde_json::Value::String("evolve".into()));
    ensure_valid_completion(&completed)?;
    atomic_write_pipeline(&pipeline_path, harness_dir, &orbit_dir, &completed)
}

fn ensure_valid_completion(state: &serde_json::Value) -> io::Result<()> {
    let violations = completion_violations(state);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Orbit completion rejected: {}", violations.join("; ")),
        ))
    }
}

fn regular_orbit_dir(harness_dir: &Path) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(harness_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "HARNESS_DIR is not a regular directory: {}",
                harness_dir.display()
            ),
        ));
    }
    let orbit_dir = harness_dir.join("orbit");
    let metadata = fs::symlink_metadata(&orbit_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Orbit directory is not a regular directory: {}",
                orbit_dir.display()
            ),
        ));
    }
    Ok(orbit_dir)
}

fn completion_candidate(
    orbit_dir: &Path,
    harness_dir: &Path,
) -> io::Result<(PathBuf, serde_json::Value)> {
    let mut running = Vec::new();
    let mut complete = Vec::new();
    for entry in fs::read_dir(orbit_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("PIPELINE-") || !name.ends_with(".json") {
            continue;
        }
        let path = entry.path();
        validate_pipeline_path(&path, harness_dir, orbit_dir)?;
        let content = fs::read_to_string(&path)?;
        let state: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Orbit pipeline {}: {error}", path.display()),
            )
        })?;
        match state.get("status").and_then(serde_json::Value::as_str) {
            Some("running") => running.push((name.to_owned(), path, state)),
            Some("complete") => complete.push((name.to_owned(), path, state)),
            _ => {}
        }
    }
    let candidates = if running.is_empty() {
        &mut complete
    } else {
        &mut running
    };
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    candidates
        .pop()
        .map(|(_, path, state)| (path, state))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no running or complete Orbit pipeline found",
            )
        })
}

fn validate_pipeline_path(path: &Path, harness_dir: &Path, orbit_dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Orbit pipeline target is not a regular file: {}",
                path.display()
            ),
        ));
    }
    let harness_root = canonical_for_compare(harness_dir)?;
    let orbit_root = canonical_for_compare(orbit_dir)?;
    let target = canonical_for_compare(path)?;
    if !target.starts_with(&harness_root) || !target.starts_with(&orbit_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Orbit pipeline target escapes HARNESS_DIR: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn atomic_write_pipeline(
    path: &Path,
    harness_dir: &Path,
    orbit_dir: &Path,
    state: &serde_json::Value,
) -> io::Result<()> {
    let payload = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Orbit pipeline target has no UTF-8 filename",
            )
        })?;
    let process_id = std::process::id();
    for attempt in 0..100u32 {
        let temporary = orbit_dir.join(format!(".{name}.{process_id}.{attempt}.tmp"));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(&payload).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        if let Err(error) = validate_pipeline_path(path, harness_dir, orbit_dir) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = crate::team::codex::atomic_replace_file(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate temporary Orbit pipeline file in {}",
            orbit_dir.display()
        ),
    ))
}

pub fn pipeline_is_dashboard_visible(pipeline: &serde_json::Value) -> bool {
    completion_violations(pipeline).is_empty()
}

/// Filter dashboard pipeline state to one project before sorting and limiting.
///
/// SQLite rows use `project`; file fallbacks use `_project`.
pub fn dashboard_pipelines_for_project(
    pipelines: Vec<serde_json::Value>,
    project: Option<&str>,
    limit: usize,
) -> Vec<serde_json::Value> {
    let mut scoped: Vec<_> = pipelines
        .into_iter()
        .filter(pipeline_is_dashboard_visible)
        .filter(|pipeline| {
            project.is_none_or(|selected| {
                pipeline
                    .get("project")
                    .or_else(|| pipeline.get("_project"))
                    .and_then(|value| value.as_str())
                    == Some(selected)
            })
        })
        .collect();
    scoped.sort_by(|left, right| {
        let left = left["started_at"].as_str().unwrap_or("");
        let right = right["started_at"].as_str().unwrap_or("");
        right.cmp(left)
    });
    scoped.truncate(limit);
    scoped
}

fn is_github_pull_request_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw.trim()) else {
        return false;
    };
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return false;
    }
    let segments: Vec<_> = parsed
        .path_segments()
        .map(|segments| segments.filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    segments.len() == 4
        && segments[2] == "pull"
        && !segments[0].is_empty()
        && !segments[1].is_empty()
        && segments[3].chars().all(|c| c.is_ascii_digit())
}

/// Sanitize a string extracted from pipeline state before emitting to LLM context.
///
/// Strips:
/// - Control characters (Unicode `Cc`: `\n`, `\r`, ESC, BEL, C1 block U+0080–U+009F)
/// - Bidirectional override/isolate characters (`Cf`: U+202A–U+202E, U+2066–U+2069)
///   which can reverse rendered text to hide prompt injection payloads
/// - Unicode line/paragraph separators (U+2028, U+2029) which act as newlines in
///   many parsers but are not caught by `is_control()`
/// - Plane-14 Unicode tag characters (U+E0000–U+E01EF), the primary LLM injection vector
///
/// Truncates to 256 Unicode scalar values.
#[allow(dead_code)]
pub fn sanitize_orbit_field(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !('\u{E0000}'..='\u{E01EF}').contains(c)
                && !('\u{202A}'..='\u{202E}').contains(c)
                && !('\u{2066}'..='\u{2069}').contains(c)
                && *c != '\u{2028}'
                && *c != '\u{2029}'
        })
        .take(256)
        .collect()
}

#[cfg(test)]
mod completion_tests {
    use super::completion_violations as violations;
    use serde_json::json;
    use std::fs;

    #[test]
    fn a_clean_completion_reports_nothing() {
        let pipeline = json!({
            "status": "complete",
            "audit_fail_count": 1,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success",
            "phase_history": [{"phase": "ship", "status": "complete"}]
        });
        assert!(violations(&pipeline).is_empty());
    }

    #[test]
    fn a_running_pipeline_is_not_checked() {
        // In-flight state is legitimately incomplete.
        let pipeline = json!({"status": "running", "audit_fail_count": 9, "max_retries": 3});
        assert!(violations(&pipeline).is_empty());
    }

    #[test]
    fn invalid_completion_is_hidden_from_dashboard_consumers() {
        let pipeline = json!({"status": "complete"});
        assert!(!super::pipeline_is_dashboard_visible(&pipeline));
    }

    #[test]
    fn dashboard_pipeline_scope_filters_before_sorting_and_limiting() {
        let pipelines = vec![
            json!({"id": "a-old", "project": "project-a", "status": "running", "started_at": "1"}),
            json!({"id": "b-new", "project": "project-b", "status": "running", "started_at": "3"}),
            json!({"id": "a-invalid", "project": "project-a", "status": "complete", "started_at": "4"}),
            json!({"id": "a-new", "project": "project-a", "status": "running", "started_at": "2"}),
        ];

        let selected =
            super::dashboard_pipelines_for_project(pipelines.clone(), Some("project-a"), 10);
        assert_eq!(
            selected
                .iter()
                .map(|pipeline| pipeline["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a-new", "a-old"]
        );

        let aggregate = super::dashboard_pipelines_for_project(pipelines, None, 1);
        assert_eq!(aggregate[0]["id"], "b-new");
    }

    #[test]
    fn exceeding_max_retries_is_reported() {
        let pipeline = json!({
            "status": "complete",
            "audit_fail_count": 5,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success"
        });
        let v = violations(&pipeline);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("audit_fail_count=5"), "{}", v[0]);
    }

    #[test]
    fn completing_without_pr_or_ci_evidence_is_reported() {
        let pipeline = json!({"status": "complete", "audit_fail_count": 0, "max_retries": 3});
        let v = violations(&pipeline);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().any(|message| message.contains("pull-request URL")));
        assert!(v.iter().any(|message| message.contains("ci_status")));
    }

    #[test]
    fn a_completed_ship_phase_is_not_pr_or_ci_evidence() {
        let pipeline = json!({
            "status": "shipped",
            "phase_history": [{"phase": "ship", "status": "complete"}]
        });
        let found = violations(&pipeline);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_blank_pr_url_is_not_evidence() {
        let pipeline = json!({"status": "complete", "pr_url": "   ", "ci_status": "success"});
        assert_eq!(violations(&pipeline).len(), 1);
    }

    #[test]
    fn a_non_pull_request_url_is_not_evidence() {
        let pipeline = json!({
            "status": "complete",
            "pr_url": "https://github.com/o/r/issues/1",
            "ci_status": "success"
        });
        assert_eq!(violations(&pipeline).len(), 1);
    }

    #[test]
    fn missing_successful_ci_is_reported() {
        let pipeline = json!({
            "status": "complete",
            "pr_url": "https://github.com/o/r/pull/1"
        });
        let found = violations(&pipeline);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("ci_status"), "{found:?}");
    }

    #[test]
    fn both_invariants_report_independently() {
        let pipeline = json!({"status": "complete", "audit_fail_count": 4, "max_retries": 3});
        assert_eq!(violations(&pipeline).len(), 3);
    }

    #[test]
    fn complete_command_commits_a_valid_running_pipeline() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-valid.json");
        fs::write(
            &pipeline,
            r#"{"id":"valid","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#,
        )
        .unwrap();

        super::complete_pipeline_in(harness.path()).unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pipeline).unwrap()).unwrap();
        assert_eq!(state["status"], "complete");
        assert_eq!(state["phase"], "evolve");
    }

    #[test]
    fn complete_command_rejects_invalid_state_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-invalid.json");
        let before = r#"{"id":"invalid","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"","ci_status":"failed"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_is_idempotent_for_a_valid_legacy_completion() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-complete.json");
        let before = r#"{"id":"complete","status":"complete","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        super::complete_pipeline_in(harness.path()).unwrap();

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    #[cfg(unix)]
    fn complete_command_rejects_a_symlinked_orbit_target() {
        let harness = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), harness.path().join("orbit")).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());
    }

    #[test]
    fn complete_command_rejects_a_pipeline_symlink_outside_harness_dir() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        let before = r#"{"id":"outside","status":"running","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(external.path(), before).unwrap();
        let link = orbit.join("PIPELINE-20260729-outside.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external.path(), &link).unwrap();
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(external.path(), &link) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("create symlink fixture: {error}"),
        }

        assert!(super::complete_pipeline_in(harness.path()).is_err());
        assert_eq!(fs::read_to_string(external.path()).unwrap(), before);
    }
}
