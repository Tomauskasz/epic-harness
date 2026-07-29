use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::paths::{canonical_for_compare, orbit_dir};

/// Maximum exact Orbit identities handled by a single SessionEnd replay.
/// Exceeding the bound fails the job rather than silently omitting state.
pub(crate) const MAX_ORBIT_PIPELINE_FILES: usize = 64;
pub(crate) const MAX_ORBIT_PIPELINE_BYTES: usize = 1024 * 1024;

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
/// Noncanonical ids are rejected rather than normalized into a colliding value.
pub fn detect_active_orbit_id() -> Option<String> {
    let val = cached_orbit_state_common()?;
    val.get("id")
        .and_then(|v| v.as_str())
        .filter(|id| is_canonical_pipeline_id(id))
        .map(str::to_owned)
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

fn is_canonical_pipeline_id(id: &str) -> bool {
    !id.is_empty() && id == normalize_pipeline_id(id)
}

/// Check the invariants a completed orbit pipeline is supposed to satisfy.
///
/// The pipeline file is written by the orbit skill, so nothing in the harness
/// could previously contradict it: pipelines were observed marked `complete`
/// with no PR or CI proof, or exhausted audit retries. Completion requires
/// explicit numeric retry evidence and an already recorded Evolve phase.
/// Consumers must run this validation before persistence so a self-declared
/// status cannot become dashboard evidence.
///
/// Returns one message per violated invariant; empty means the state is
/// self-consistent. Pipelines that are not complete are not checked — an
/// in-flight pipeline is legitimately missing most of this.
/// Check completion invariants with separately verified SessionEnd evidence.
/// A nonempty `evolution_session_id` is only self-assertion; callers that make
/// a persistence or visibility decision must pass whether the exact
/// `(project, session_id, pipeline_id)` mapping exists in durable storage.
pub fn completion_violations_with_durable_evolution(
    pipeline: &serde_json::Value,
    durable_evolution: bool,
) -> Vec<String> {
    let status = pipeline.get("status").and_then(|v| v.as_str());
    if !matches!(status, Some("complete") | Some("shipped")) {
        return Vec::new();
    }

    let mut violations = Vec::new();
    let retry_count = pipeline
        .get("audit_fail_count")
        .and_then(serde_json::Value::as_u64);
    let max_retries = pipeline
        .get("max_retries")
        .and_then(serde_json::Value::as_u64);
    match (retry_count, max_retries) {
        (Some(fails), Some(max)) if fails >= max => violations.push(format!(
            "completed with audit_fail_count={fails} at or above max_retries={max}; the run should have paused for a decision"
        )),
        (Some(_), Some(_)) => {}
        _ => violations.push(
            "completed without integer audit_fail_count and max_retries evidence".to_string(),
        ),
    }

    if pipeline.get("phase").and_then(|v| v.as_str()) != Some("evolve") {
        violations.push("completed without phase=\"evolve\" evidence".to_string());
    }

    if let Some(last_evolve) = pipeline
        .get("phase_history")
        .and_then(serde_json::Value::as_array)
        .and_then(|history| {
            history
                .iter()
                .rfind(|entry| entry.get("phase").and_then(|v| v.as_str()) == Some("evolve"))
        })
        && last_evolve.get("status").and_then(|v| v.as_str()) != Some("complete")
    {
        violations.push("completed after a failed Evolve phase".to_string());
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

    if pipeline
        .get("evolution_session_id")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|session_id| session_id.trim().is_empty())
    {
        violations.push("completed without durable SessionEnd evolution evidence".to_string());
    } else if !durable_evolution {
        violations.push(
            "completed with an evolution_session_id that has no durable SessionEnd pipeline record"
                .to_string(),
        );
    }

    violations
}

/// Reject manual Orbit completion because it has no validated SessionEnd identity.
pub fn complete_pipeline_in(harness_dir: &Path) -> io::Result<()> {
    let _ = harness_dir;
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Orbit completion is reserved for durable SessionEnd reflection",
    ))
}

/// Atomically finish only pipelines observed in a SessionEnd reflection after
/// that reflection's durable completion record has been committed.
pub fn complete_pipelines_after_reflection_in(
    harness_dir: &Path,
    reflection_session_id: &str,
    project: &str,
    pipeline_ids: &[String],
) -> io::Result<usize> {
    if reflection_session_id.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection session identity is empty",
        ));
    }
    let durable_pipeline_ids = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::reflection_pipeline_ids_pool(&pool, reflection_session_id, project)
            .await
    })?;
    let mut requested_pipeline_ids = pipeline_ids.to_vec();
    requested_pipeline_ids.sort();
    requested_pipeline_ids.dedup();
    if durable_pipeline_ids != requested_pipeline_ids {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Orbit completion ids do not exactly match durable SessionEnd evidence",
        ));
    }
    complete_pipelines_after_verified_reflection_in(
        harness_dir,
        reflection_session_id,
        &requested_pipeline_ids,
    )
}

/// Select exact observed pipeline ids that are ready for SessionEnd evolution.
/// Ordinary in-flight phases are intentionally ignored: observing a pipeline
/// while it is in `go` or `ship` must not turn it into a durable completion
/// candidate. A matching `awaiting_evolution` pipeline is fully validated
/// before the reflection completion marker is allowed to record its id.
pub fn reflection_completion_candidates_in(
    harness_dir: &Path,
    observed_pipeline_ids: &[String],
) -> io::Result<Vec<String>> {
    if observed_pipeline_ids.is_empty() {
        return Ok(Vec::new());
    }
    let observed: std::collections::BTreeSet<&str> =
        observed_pipeline_ids.iter().map(String::as_str).collect();
    if observed.len() > MAX_ORBIT_PIPELINE_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SessionEnd observed more than {MAX_ORBIT_PIPELINE_FILES} Orbit pipeline identities"
            ),
        ));
    }
    let mut candidates = std::collections::BTreeSet::new();
    for id in observed {
        if !is_canonical_pipeline_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Orbit pipeline id must be canonical before SessionEnd completion",
            ));
        }
        let Some((_, state)) = read_exact_orbit_pipeline_in(harness_dir, id)? else {
            continue;
        };
        if state.get("status").and_then(serde_json::Value::as_str) != Some("running") {
            continue;
        }
        if state.get("phase").and_then(serde_json::Value::as_str) != Some("awaiting_evolution") {
            continue;
        }
        validate_ready_for_reflection_completion(&state)?;
        candidates.insert(id.to_string());
    }
    Ok(candidates.into_iter().collect())
}

fn complete_pipelines_after_verified_reflection_in(
    harness_dir: &Path,
    reflection_session_id: &str,
    pipeline_ids: &[String],
) -> io::Result<usize> {
    let orbit_dir = regular_orbit_dir(harness_dir)?;
    let expected_ids: std::collections::BTreeSet<&str> =
        pipeline_ids.iter().map(String::as_str).collect();
    if expected_ids.len() > MAX_ORBIT_PIPELINE_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SessionEnd recorded more than {MAX_ORBIT_PIPELINE_FILES} Orbit pipeline identities"
            ),
        ));
    }
    let mut pending = Vec::new();
    for id in expected_ids {
        if !is_canonical_pipeline_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Orbit pipeline id must be canonical before SessionEnd completion",
            ));
        }
        let Some((path, state)) = read_exact_orbit_pipeline_in(harness_dir, id)? else {
            continue;
        };
        if state.get("status").and_then(serde_json::Value::as_str) != Some("running") {
            continue;
        }
        validate_ready_for_reflection_completion(&state)?;
        pending.push((path, state));
    }

    let mut completed = 0;
    for (path, mut state) in pending {
        let object = state.as_object_mut().ok_or_else(|| {
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
        object.insert(
            "evolution_session_id".into(),
            serde_json::Value::String(reflection_session_id.into()),
        );
        ensure_valid_completion(&state, true)?;
        atomic_write_pipeline(&path, harness_dir, &orbit_dir, &state)?;
        completed += 1;
    }
    Ok(completed)
}

fn validate_ready_for_reflection_completion(state: &serde_json::Value) -> io::Result<()> {
    if state.get("phase").and_then(serde_json::Value::as_str) != Some("awaiting_evolution") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Orbit pipeline must explicitly be awaiting_evolution before SessionEnd completion",
        ));
    }
    if !matches!(
        state.get("phase_history"),
        Some(serde_json::Value::Array(_))
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Orbit pipeline phase_history must already be an array",
        ));
    }
    let mut completed = state.clone();
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
    object.insert(
        "evolution_session_id".into(),
        serde_json::Value::String("validated-session".into()),
    );
    ensure_valid_completion(&completed, true)
}

fn ensure_valid_completion(state: &serde_json::Value, durable_evolution: bool) -> io::Result<()> {
    let violations = completion_violations_with_durable_evolution(state, durable_evolution);
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

/// Read exactly the filename owned by one canonical pipeline identity.
/// Pipeline state has always been persisted as `PIPELINE-{id}.json`; enforcing
/// that relationship lets SessionEnd avoid parsing unrelated files.
pub(crate) fn read_exact_orbit_pipeline_in(
    harness_dir: &Path,
    pipeline_id: &str,
) -> io::Result<Option<(PathBuf, serde_json::Value)>> {
    if !is_canonical_pipeline_id(pipeline_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Orbit pipeline id must be canonical",
        ));
    }
    let orbit_dir = regular_orbit_dir(harness_dir)?;
    let path = orbit_dir.join(format!("PIPELINE-{pipeline_id}.json"));
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    validate_pipeline_path(&path, harness_dir, &orbit_dir)?;
    let mut bytes = Vec::with_capacity(MAX_ORBIT_PIPELINE_BYTES.min(64 * 1024));
    fs::File::open(&path)?
        .take((MAX_ORBIT_PIPELINE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ORBIT_PIPELINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Orbit pipeline exceeds {MAX_ORBIT_PIPELINE_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let state: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Orbit pipeline {}: {error}", path.display()),
        )
    })?;
    if state.get("id").and_then(serde_json::Value::as_str) != Some(pipeline_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Orbit pipeline filename and id disagree: expected {pipeline_id} in {}",
                path.display()
            ),
        ));
    }
    Ok(Some((path, state)))
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
        return sync_orbit_directory(orbit_dir);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate temporary Orbit pipeline file in {}",
            orbit_dir.display()
        ),
    ))
}

fn sync_orbit_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub fn pipeline_is_dashboard_visible(pipeline: &serde_json::Value) -> bool {
    let completed = matches!(
        pipeline.get("status").and_then(serde_json::Value::as_str),
        Some("complete") | Some("shipped")
    );
    completion_violations_with_durable_evolution(
        pipeline,
        !completed
            || pipeline
                .get("_durable_evolution")
                .and_then(serde_json::Value::as_bool)
                == Some(true),
    )
    .is_empty()
        && (!completed
            || pipeline
                .get("_durable_evolution")
                .and_then(serde_json::Value::as_bool)
                == Some(true))
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
    use serde_json::json;
    use std::fs;

    fn violations(pipeline: &serde_json::Value) -> Vec<String> {
        super::completion_violations_with_durable_evolution(pipeline, true)
    }

    #[test]
    fn a_clean_completion_reports_nothing() {
        let pipeline = json!({
            "status": "complete",
            "phase": "evolve",
            "audit_fail_count": 1,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success",
            "evolution_session_id": "20260729_host",
            "phase_history": [{"phase": "ship", "status": "complete"}]
        });
        assert!(violations(&pipeline).is_empty());
    }

    #[test]
    fn a_self_asserted_evolution_session_is_not_durable_completion_evidence() {
        let pipeline = json!({
            "status": "complete",
            "phase": "evolve",
            "audit_fail_count": 0,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success",
            "evolution_session_id": "invented-session",
            "phase_history": []
        });

        let violations = super::completion_violations_with_durable_evolution(&pipeline, false);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("no durable SessionEnd pipeline record")),
            "{violations:?}"
        );
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
    fn completed_pipeline_without_durable_mapping_is_hidden_from_dashboard_consumers() {
        let pipeline = json!({
            "status": "complete",
            "phase": "evolve",
            "audit_fail_count": 0,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success",
            "evolution_session_id": "invented-session",
            "phase_history": []
        });
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
            "phase": "evolve",
            "audit_fail_count": 5,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "ci_status": "success",
            "evolution_session_id": "20260729_host"
        });
        let v = violations(&pipeline);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("audit_fail_count=5"), "{}", v[0]);
    }

    #[test]
    fn completing_without_pr_or_ci_evidence_is_reported() {
        let pipeline = json!({"status": "complete", "phase": "evolve", "audit_fail_count": 0, "max_retries": 3, "evolution_session_id": "20260729_host"});
        let v = violations(&pipeline);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().any(|message| message.contains("pull-request URL")));
        assert!(v.iter().any(|message| message.contains("ci_status")));
    }

    #[test]
    fn a_completed_ship_history_is_not_pr_or_ci_evidence() {
        let pipeline = json!({
            "status": "shipped",
            "phase": "evolve",
            "audit_fail_count": 0,
            "max_retries": 3,
            "evolution_session_id": "20260729_host",
            "phase_history": [{"phase": "ship", "status": "complete"}]
        });
        let found = violations(&pipeline);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_blank_pr_url_is_not_evidence() {
        let pipeline = json!({"status": "complete", "phase": "evolve", "audit_fail_count": 0, "max_retries": 3, "pr_url": "   ", "ci_status": "success", "evolution_session_id": "20260729_host"});
        assert_eq!(violations(&pipeline).len(), 1);
    }

    #[test]
    fn a_non_pull_request_url_is_not_evidence() {
        let pipeline = json!({
            "status": "complete",
            "phase": "evolve",
            "audit_fail_count": 0,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/issues/1",
            "ci_status": "success",
            "evolution_session_id": "20260729_host"
        });
        assert_eq!(violations(&pipeline).len(), 1);
    }

    #[test]
    fn missing_successful_ci_is_reported() {
        let pipeline = json!({
            "status": "complete",
            "phase": "evolve",
            "audit_fail_count": 0,
            "max_retries": 3,
            "pr_url": "https://github.com/o/r/pull/1",
            "evolution_session_id": "20260729_host"
        });
        let found = violations(&pipeline);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("ci_status"), "{found:?}");
    }

    #[test]
    fn completion_evidence_violations_report_independently() {
        let pipeline = json!({"status": "complete", "phase": "evolve", "audit_fail_count": 4, "max_retries": 3, "evolution_session_id": "20260729_host"});
        assert_eq!(violations(&pipeline).len(), 3);
    }

    #[test]
    fn reflection_completion_commits_an_observed_running_pipeline() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-valid.json");
        fs::write(
            &pipeline,
            r#"{"id":"valid","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#,
        )
        .unwrap();

        assert_eq!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["valid".into()],
            )
            .unwrap(),
            1,
        );

        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pipeline).unwrap()).unwrap();
        assert_eq!(state["status"], "complete");
        assert_eq!(state["phase"], "evolve");
        assert_eq!(state["evolution_session_id"], "20260729_host");
    }

    #[test]
    fn reflection_completion_rejects_a_pipeline_that_is_not_awaiting_evolution_without_changing_bytes()
     {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-not-ready.json");
        let before = r#"{"id":"not-ready","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["not-ready".into()],
            )
            .is_err()
        );

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn reflection_completion_requires_existing_phase_history_without_fabricating_one() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-no-history.json");
        let before = r#"{"id":"no-history","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["no-history".into()],
            )
            .is_err()
        );

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn reflection_completion_matches_the_exact_observed_pipeline_id() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-a-b.json");
        let before = r#"{"id":"a-b","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, before).unwrap();

        assert_eq!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["a_b".into()],
            )
            .unwrap(),
            0
        );

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn candidate_selection_ignores_in_flight_phases_then_accepts_awaiting_evolution() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-later.json");
        let in_flight = r#"{"id":"later","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, in_flight).unwrap();

        assert_eq!(
            super::reflection_completion_candidates_in(harness.path(), &["later".into()]).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(fs::read_to_string(&pipeline).unwrap(), in_flight);

        fs::write(
            &pipeline,
            r#"{"id":"later","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#,
        )
        .unwrap();
        assert_eq!(
            super::reflection_completion_candidates_in(harness.path(), &["later".into()]).unwrap(),
            vec!["later"]
        );
    }

    #[test]
    fn candidate_selection_without_observed_pipeline_does_not_require_an_orbit_directory() {
        let harness = tempfile::tempdir().unwrap();

        assert_eq!(
            super::reflection_completion_candidates_in(harness.path(), &[]).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn candidate_selection_rejects_noncanonical_id_without_changing_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-noncanonical.json");
        let before = r#"{"id":"a?b","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(
            super::reflection_completion_candidates_in(harness.path(), &["a?b".into()],).is_err()
        );
        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn candidate_selection_rejects_malformed_ready_pipeline_without_changing_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-malformed.json");
        let before = r#"{"id":"malformed","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":{}}"#;
        fs::write(&pipeline, before).unwrap();

        let error =
            super::reflection_completion_candidates_in(harness.path(), &["malformed".into()])
                .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn candidate_selection_ignores_an_unobserved_malformed_pipeline() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        fs::write(
            orbit.join("PIPELINE-ready.json"),
            r#"{"id":"ready","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#,
        )
        .unwrap();
        let unrelated = orbit.join("PIPELINE-unrelated.json");
        let malformed = "{";
        fs::write(&unrelated, malformed).unwrap();

        assert_eq!(
            super::reflection_completion_candidates_in(harness.path(), &["ready".into()]).unwrap(),
            vec!["ready"]
        );
        assert_eq!(fs::read_to_string(unrelated).unwrap(), malformed);
    }

    #[test]
    fn candidate_selection_rejects_more_than_the_pipeline_cap() {
        let harness = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (0..=super::MAX_ORBIT_PIPELINE_FILES)
            .map(|index| format!("pipeline-{index}"))
            .collect();

        let error = super::reflection_completion_candidates_in(harness.path(), &ids).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn candidate_selection_rejects_an_oversized_exact_pipeline_without_changing_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-ready.json");
        let bytes = vec![b' '; super::MAX_ORBIT_PIPELINE_BYTES + 1];
        fs::write(&pipeline, &bytes).unwrap();

        let error = super::reflection_completion_candidates_in(harness.path(), &["ready".into()])
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read(pipeline).unwrap(), bytes);
    }

    #[test]
    fn candidate_selection_rejects_a_filename_and_id_mismatch_without_changing_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-duplicate.json");
        let state = r#"{"id":"other","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, state).unwrap();

        assert!(
            super::reflection_completion_candidates_in(harness.path(), &["duplicate".into()])
                .is_err()
        );
        assert_eq!(fs::read_to_string(pipeline).unwrap(), state);
    }

    #[test]
    fn completion_rejects_a_filename_and_id_mismatch_without_changing_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-duplicate.json");
        let state = r#"{"id":"other","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, state).unwrap();

        assert!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729-session",
                &["duplicate".into()],
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(pipeline).unwrap(), state);
    }

    #[test]
    fn completion_ignores_an_unobserved_noncanonical_running_pipeline() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let ready = orbit.join("PIPELINE-ready.json");
        let unrelated = orbit.join("PIPELINE-20260729-unrelated.json");
        fs::write(
            &ready,
            r#"{"id":"ready","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#,
        )
        .unwrap();
        let unrelated_before = r#"{"id":"a?b","status":"running","phase":"go"}"#;
        fs::write(&unrelated, unrelated_before).unwrap();

        assert_eq!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729-session",
                &["ready".into()],
            )
            .unwrap(),
            1
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(ready).unwrap()).unwrap()
                ["evolution_session_id"],
            "20260729-session"
        );
        assert_eq!(fs::read_to_string(unrelated).unwrap(), unrelated_before);
    }

    #[test]
    fn reflection_completion_retry_keeps_the_same_exact_pipeline_id_after_a_rejected_write() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-retry-id.json");
        let rejected = r#"{"id":"retry-id","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#;
        fs::write(&pipeline, rejected).unwrap();

        assert!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["retry-id".into()],
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&pipeline).unwrap(), rejected);

        fs::write(
            &pipeline,
            r#"{"id":"retry-id","status":"running","phase":"awaiting_evolution","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[]}"#,
        )
        .unwrap();

        assert_eq!(
            super::complete_pipelines_after_verified_reflection_in(
                harness.path(),
                "20260729_host",
                &["retry-id".into()],
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn complete_command_rejects_missing_retry_evidence_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-missing-retries.json");
        let before = r#"{"id":"missing-retries","status":"running","phase":"evolve","audit_fail_count":1,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_rejects_absent_retry_evidence_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-absent-retries.json");
        let before = r#"{"id":"absent-retries","status":"running","phase":"evolve","pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_rejects_malformed_retry_evidence_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-malformed-retries.json");
        let before = r#"{"id":"malformed-retries","status":"running","phase":"evolve","audit_fail_count":1,"max_retries":"3","pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_rejects_retry_count_at_the_limit_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-retry-limit.json");
        let before = r#"{"id":"retry-limit","status":"running","phase":"evolve","audit_fail_count":3,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_requires_evolve_phase_evidence_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-ship-phase.json");
        let before = r#"{"id":"ship-phase","status":"running","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }

    #[test]
    fn complete_command_rejects_failed_evolve_history_without_changing_its_bytes() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-failed-evolve.json");
        let before = r#"{"id":"failed-evolve","status":"running","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","phase_history":[{"phase":"evolve","status":"failed"}]}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
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
    fn manual_completion_rejects_even_a_valid_legacy_completion() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-complete.json");
        let before = r#"{"id":"complete","status":"complete","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&pipeline, before).unwrap();

        assert!(super::complete_pipeline_in(harness.path()).is_err());

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
