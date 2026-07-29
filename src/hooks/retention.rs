//! retention.rs — Age out observation rows and the runtime files keyed on them.
//!
//! Nothing deleted observations before this: `delete_obs_older_than_pool` existed
//! but had no caller outside its own test, so the table only ever grew. The
//! per-session runtime files had the same problem, made worse by the old
//! PID-derived session id — a hook runs in its own process, so every tool call
//! could leave a fresh one-byte telemetry counter and a stale resume lock.
//!
//! Runs at session end, after reflect has analyzed the day.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};
use std::{collections::HashSet, fs::OpenOptions};

use serde::Deserialize;

use super::common::*;

/// Runtime files are dropped once they are older than this. Well past any live
/// session, so an in-flight lock is never removed.
const RUNTIME_FILE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const GLOBAL_RETENTION_CADENCE: Duration = Duration::from_secs(60 * 60);
/// Retention runs synchronously at session end, so hostile local state must
/// not make it enumerate or deserialize without a finite bound.
const MAX_RETENTION_PROJECTS: usize = 256;
const MAX_REFLECTION_QUEUE_ENTRIES: usize = 512;
const MAX_ACTIVE_REFLECTION_JOBS: usize = 256;
const MAX_REFLECTION_JOB_BYTES: usize = 64 * 1024;

/// Held OS lock for one global retention sweep.
///
/// The `retention.lock` pathname intentionally persists: closing this handle
/// releases ownership, including after process death, without ever unlinking a
/// successor's lock file.
#[derive(Debug)]
struct RetentionLease {
    _file: fs::File,
}

/// An opened projects directory. Unix children are always resolved relative to
/// this descriptor, so replacing the pathname after this point cannot redirect
/// a marker or lock operation.
struct RetentionRoot {
    #[cfg(unix)]
    directory: fs::File,
    #[cfg(windows)]
    // Kept open for the whole operation. `std` has no Windows equivalent of
    // `openat`, so child lookup still cannot prove an ancestor was not
    // replaced after root validation; every opened leaf is nevertheless a
    // `FILE_FLAG_OPEN_REPARSE_POINT` handle and is rejected before mutation.
    _directory: fs::File,
    #[cfg(windows)]
    path: std::path::PathBuf,
}

#[derive(Deserialize)]
struct QueuedReflectionJob {
    session_id: String,
}

/// Run retention without deleting the session whose SessionEnd reflection is
/// about to read it. Long-running sessions can legitimately predate the
/// configured cutoff.
pub(crate) fn run_preserving_session(
    active_session: Option<(&str, &str)>,
) -> io::Result<(u64, usize)> {
    let projects = harness_projects_root();
    let harness = harness_dir();
    let obs = obs_dir();
    run_preserving_session_at(
        active_session,
        crate::config::CONFIG.db.retention_days,
        &projects,
        &harness,
        &obs,
    )
}

fn run_preserving_session_at(
    active_session: Option<(&str, &str)>,
    days: u64,
    projects: &Path,
    harness: &Path,
    obs: &Path,
) -> io::Result<(u64, usize)> {
    if days == 0 {
        return Ok((0, 0));
    }

    let now = SystemTime::now();
    let Some(_lease) = try_acquire_global_retention_lease(projects, now)? else {
        return Ok((0, 0));
    };

    // Complete every bounded, fallible scan before any delete/prune operation.
    let active_sessions = active_reflection_sessions(projects, active_session)?;
    let mut files = sweep_runtime_files(harness, obs, now);

    let cutoff_day = days_ago(days);
    let rows = delete_old_rows(&cutoff_day, &active_sessions)?;
    files += prune_observation_jsonl_excluding(projects, &cutoff_day, &active_sessions)?;
    files += prune_completed_reflection_jobs(
        projects,
        Duration::from_secs(days.saturating_mul(24 * 60 * 60)),
        now,
    )?;
    record_global_retention(projects)?;
    Ok((rows, files))
}

fn delete_old_rows(
    cutoff_day: &str,
    active_sessions: &HashSet<(String, String)>,
) -> io::Result<u64> {
    // `timestamp` holds ISO text and is compared lexicographically, so the
    // cutoff has to be in the same shape the column stores.
    let cutoff = format!("{}T00:00:00", iso_day(cutoff_day));
    crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        let sessions = active_sessions.iter().cloned().collect::<Vec<_>>();
        crate::store::observations::delete_obs_older_than_except_sessions_pool(
            &pool, &cutoff, &sessions,
        )
        .await
    })
}

/// `YYYYMMDD` → `YYYY-MM-DD`. Any other shape passes through unchanged.
fn iso_day(raw: &str) -> String {
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..8])
    } else {
        raw.to_string()
    }
}

/// Remove fallback observation logs older than `cutoff_day` across every
/// project. Session filenames begin with `session_YYYYMMDD_`; names with any
/// other shape are unrelated data and remain untouched.
#[cfg(test)]
pub(crate) fn prune_observation_jsonl(
    projects: &Path,
    cutoff_day: &str,
    active_session: Option<(&str, &str)>,
) -> io::Result<usize> {
    let active_sessions = active_session
        .map(|(session_id, project)| HashSet::from([(session_id.to_string(), project.to_string())]))
        .unwrap_or_default();
    prune_observation_jsonl_excluding(projects, cutoff_day, &active_sessions)
}

fn prune_observation_jsonl_excluding(
    projects: &Path,
    cutoff_day: &str,
    active_sessions: &HashSet<(String, String)>,
) -> io::Result<usize> {
    let mut removed = 0;
    let entries = match fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    for project in entries {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let obs = project.path().join("obs");
        if obs
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        let obs_entries = match fs::read_dir(&obs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in obs_entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(day) = name
                .strip_prefix("session_")
                .and_then(|rest| rest.get(..8))
                .filter(|day| day.chars().all(|c| c.is_ascii_digit()))
            else {
                continue;
            };
            if !name.ends_with(".jsonl") || day >= cutoff_day {
                continue;
            }
            let project_name = project.file_name().to_string_lossy().into_owned();
            let session_id = name
                .strip_prefix("session_")
                .and_then(|value| value.strip_suffix(".jsonl"))
                .unwrap_or_default();
            if active_sessions.contains(&(session_id.to_string(), project_name)) {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

fn try_acquire_global_retention_lease(
    projects: &Path,
    now: SystemTime,
) -> io::Result<Option<RetentionLease>> {
    let root = open_retention_root(projects)?;
    let marker_is_recent = match open_retention_child(&root, "retention.last", false, false) {
        Ok(file) => file
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age < GLOBAL_RETENTION_CADENCE),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if marker_is_recent {
        return Ok(None);
    }

    let file = open_retention_child(&root, "retention.lock", true, true)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(RetentionLease { _file: file })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn record_global_retention(projects: &Path) -> io::Result<()> {
    let root = open_retention_root(projects)?;
    record_global_retention_in_root(&root)
}

fn record_global_retention_in_root(root: &RetentionRoot) -> io::Result<()> {
    // Do not request truncation during open: it would be applied before a
    // Windows reparse-point handle can be verified. The opened handle remains
    // stable if a pathname is swapped after this point.
    let mut file = open_retention_child(root, "retention.last", true, true)?;
    file.set_len(0)?;
    file.write_all(crate::shared::helpers::now_iso().as_bytes())?;
    file.sync_all()
}

#[cfg(all(test, unix))]
fn record_global_retention_after_root_open(
    projects: &Path,
    before_child_open: impl FnOnce(),
) -> io::Result<()> {
    let root = open_retention_root(projects)?;
    before_child_open();
    record_global_retention_in_root(&root)
}

fn open_retention_root(projects: &Path) -> io::Result<RetentionRoot> {
    match fs::create_dir_all(projects) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(projects)?;
        if !directory.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("retention root is not a directory: {}", projects.display()),
            ));
        }
        return Ok(RetentionRoot { directory });
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(projects)?;
        let metadata = directory.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "retention root is a reparse point or not a directory: {}",
                    projects.display()
                ),
            ));
        }
        return Ok(RetentionRoot {
            _directory: directory,
            path: projects.to_path_buf(),
        });
    }

    #[cfg(not(any(unix, windows)))]
    {
        let metadata = fs::symlink_metadata(projects)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("retention root is not a directory: {}", projects.display()),
            ));
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "retention needs a no-follow directory open on this platform",
        ))
    }
}

/// Open a regular retention child without following a leaf symlink/reparse
/// point. On Unix, `openat` also binds child lookup to the opened root.
fn open_retention_child(
    root: &RetentionRoot,
    name: &str,
    write: bool,
    create: bool,
) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let name = CString::new(name).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "retention child contains NUL")
        })?;
        let mut flags = libc::O_CLOEXEC | libc::O_NOFOLLOW;
        flags |= if write { libc::O_RDWR } else { libc::O_RDONLY };
        if create {
            flags |= libc::O_CREAT;
        }
        let fd = unsafe { libc::openat(root.directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retention child is not a regular file",
            ));
        }
        return Ok(file);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        let mut options = OpenOptions::new();
        options
            .read(!write)
            .write(write)
            .create(create)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(root.path.join(name))?;
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retention child is a reparse point or not a regular file",
            ));
        }
        return Ok(file);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, name, write, create);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "retention needs a no-follow child open on this platform",
        ))
    }
}

fn active_reflection_sessions(
    projects: &Path,
    active_session: Option<(&str, &str)>,
) -> io::Result<HashSet<(String, String)>> {
    let mut active = HashSet::new();
    if let Some((session_id, project)) = active_session {
        active.insert((session_id.to_string(), project.to_string()));
    }
    let entries = match fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(active),
        Err(error) => return Err(error),
    };
    for (project_count, project) in entries.enumerate() {
        if project_count >= MAX_RETENTION_PROJECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("retention projects exceed limit of {MAX_RETENTION_PROJECTS}"),
            ));
        }
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let project_name = project.file_name().to_string_lossy().into_owned();
        let queue = project.path().join("reflect-queue");
        if queue
            .symlink_metadata()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        let jobs = match fs::read_dir(&queue) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut valid_jobs = 0;
        for (queue_entry_count, job) in jobs.enumerate() {
            if queue_entry_count >= MAX_REFLECTION_QUEUE_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "reflection queue entries exceed limit of {MAX_REFLECTION_QUEUE_ENTRIES}: {}",
                        queue.display()
                    ),
                ));
            }
            let job = job?;
            if !job.file_type()?.is_file() {
                continue;
            }
            let path = job.path();
            if !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("pending") | Some("claimed")
            ) {
                continue;
            }
            valid_jobs += 1;
            if valid_jobs > MAX_ACTIVE_REFLECTION_JOBS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "valid reflection jobs exceed limit of {MAX_ACTIVE_REFLECTION_JOBS}: {}",
                        queue.display()
                    ),
                ));
            }
            let queued: QueuedReflectionJob =
                serde_json::from_slice(&read_reflection_job(&path)?).map_err(io::Error::other)?;
            if queued.session_id.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("reflection job has empty session id: {}", path.display()),
                ));
            }
            active.insert((queued.session_id, project_name.clone()));
        }
    }
    Ok(active)
}

fn read_reflection_job(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("reflection job is not a regular file: {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reflection job is a reparse point: {}", path.display()),
            ));
        }
    }
    if metadata.len() > MAX_REFLECTION_JOB_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reflection job exceeds {MAX_REFLECTION_JOB_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_REFLECTION_JOB_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REFLECTION_JOB_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reflection job grew beyond {MAX_REFLECTION_JOB_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    Ok(bytes)
}

pub(crate) fn prune_completed_reflection_jobs(
    projects: &Path,
    max_age: Duration,
    now: SystemTime,
) -> io::Result<usize> {
    let entries = match fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for project in entries {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let queue = project.path().join("reflect-queue");
        if queue
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        let jobs = match fs::read_dir(queue) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for job in jobs {
            let job = job?;
            if !job.file_type()?.is_file() {
                continue;
            }
            let name = job.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("job_") || !name.ends_with(".completed") {
                continue;
            }
            let modified = job.metadata()?.modified()?;
            if now
                .duration_since(modified)
                .map(|age| age > max_age)
                .unwrap_or(false)
            {
                match fs::remove_file(job.path()) {
                    Ok(()) => removed += 1,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(removed)
}

/// Remove per-session scratch files that no live session can still be using.
///
/// Covers the two that accumulate one-per-hook-process: telemetry error
/// counters in `obs/`, plus resume locks and event markers in the harness root.
/// `now` is a parameter so a test can move time forward instead of back-dating files.
pub(crate) fn sweep_runtime_files(harness: &Path, obs: &Path, now: SystemTime) -> usize {
    let stale = |p: &Path| -> bool {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| {
                now.duration_since(t)
                    .map(|age| age > RUNTIME_FILE_MAX_AGE)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    };

    let mut removed = 0;
    let targets = [
        (harness, "resume.", ".lock"),
        (harness, "resume.", ".event"),
        (obs, "telemetry_error_count_", ".txt"),
    ];
    for (dir, prefix, suffix) in targets {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(prefix) || !name.ends_with(suffix) {
                continue;
            }
            let path = entry.path();
            // Never follow a symlink out of the harness directory.
            let is_regular = path
                .symlink_metadata()
                .map(|m| m.file_type().is_file())
                .unwrap_or(false);
            if !is_regular {
                continue;
            }
            if stale(&path) && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    /// Two days past the files just created, so they read as stale.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(48 * 60 * 60)
    }

    #[test]
    fn iso_day_expands_compact_dates() {
        assert_eq!(iso_day("20260727"), "2026-07-27");
        assert_eq!(iso_day("2026-07-27"), "2026-07-27");
    }

    #[test]
    fn sweep_removes_stale_per_session_scratch_files() {
        let dir = tempdir().unwrap();
        let harness = dir.path();
        let obs = harness.join("obs");
        fs::create_dir(&obs).unwrap();

        let lock = harness.join("resume.20260101_1234.lock");
        let event = harness.join("resume.20260101_1234.session-start.event");
        let counter = obs.join("telemetry_error_count_20260101_1234.txt");
        let unrelated = harness.join("config.toml");
        for p in [&lock, &event, &counter, &unrelated] {
            File::create(p).unwrap();
        }

        assert_eq!(sweep_runtime_files(harness, &obs, later()), 3);
        assert!(!lock.exists());
        assert!(!event.exists());
        assert!(!counter.exists());
        assert!(unrelated.exists(), "unrelated files must not be touched");
    }

    #[test]
    fn sweep_keeps_files_a_live_session_may_hold() {
        let dir = tempdir().unwrap();
        let harness = dir.path();
        let obs = harness.join("obs");
        fs::create_dir(&obs).unwrap();

        let lock = harness.join("resume.20260727_9999.lock");
        File::create(&lock).unwrap();

        assert_eq!(sweep_runtime_files(harness, &obs, SystemTime::now()), 0);
        assert!(lock.exists());
    }

    #[test]
    fn sweep_tolerates_missing_directories() {
        let dir = tempdir().unwrap();
        assert_eq!(
            sweep_runtime_files(
                &dir.path().join("nope"),
                &dir.path().join("also-nope"),
                later()
            ),
            0
        );
    }

    #[test]
    fn zero_day_retention_keeps_runtime_observations_and_completed_jobs() {
        let dir = tempdir().unwrap();
        let harness = dir.path().join("harness");
        let obs = harness.join("obs");
        let projects = dir.path().join("projects");
        let project_obs = projects.join("project-a").join("obs");
        let queue = projects.join("project-a").join("reflect-queue");
        fs::create_dir_all(&obs).unwrap();
        fs::create_dir_all(&project_obs).unwrap();
        fs::create_dir_all(&queue).unwrap();

        let runtime_lock = harness.join("resume.20260101_stale.lock");
        let runtime_event = harness.join("resume.20260101_stale.event");
        let telemetry = obs.join("telemetry_error_count_20260101_stale.txt");
        let observation = project_obs.join("session_20260101_stale.jsonl");
        let completed_job = queue.join("job_20260101_stale.completed");
        for path in [
            &runtime_lock,
            &runtime_event,
            &telemetry,
            &observation,
            &completed_job,
        ] {
            File::create(path).unwrap();
        }

        let retained = run_preserving_session_at(None, 0, &projects, &harness, &obs).unwrap();

        assert_eq!(retained, (0, 0));
        for path in [
            &runtime_lock,
            &runtime_event,
            &telemetry,
            &observation,
            &completed_job,
        ] {
            assert!(
                path.exists(),
                "zero-day retention must keep {}",
                path.display()
            );
        }
        assert!(
            !projects.join("retention.lock").exists(),
            "disabled retention must not acquire a sweep lease"
        );
    }

    #[test]
    fn fallback_jsonl_retention_prunes_all_projects_at_cutoff() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        for slug in ["project-a", "project-b"] {
            fs::create_dir_all(projects.join(slug).join("obs")).unwrap();
            File::create(
                projects
                    .join(slug)
                    .join("obs")
                    .join("session_20260430_session.jsonl"),
            )
            .unwrap();
        }
        let boundary = projects
            .join("project-a")
            .join("obs")
            .join("session_20260501_session.jsonl");
        let unrelated = projects.join("project-a").join("obs").join("notes.jsonl");
        File::create(&boundary).unwrap();
        File::create(&unrelated).unwrap();

        let removed = prune_observation_jsonl(&projects, "20260501", None).unwrap();

        assert_eq!(removed, 2);
        assert!(boundary.exists(), "cutoff day is retained");
        assert!(
            unrelated.exists(),
            "non-session JSONL is not retention data"
        );
    }

    #[test]
    fn completed_reflection_markers_expire_but_pending_jobs_remain() {
        let dir = tempdir().unwrap();
        let queue = dir.path().join("project-a").join("reflect-queue");
        fs::create_dir_all(&queue).unwrap();
        let completed = queue.join("job_session.completed");
        let pending = queue.join("job_session.pending");
        File::create(&completed).unwrap();
        File::create(&pending).unwrap();

        let removed =
            prune_completed_reflection_jobs(dir.path(), Duration::from_secs(24 * 60 * 60), later())
                .unwrap();

        assert_eq!(removed, 1);
        assert!(!completed.exists());
        assert!(pending.exists(), "unfinished work must remain durable");
    }

    #[test]
    fn fallback_retention_preserves_ending_session_and_skips_symlinked_obs() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        let obs = project.join("obs");
        fs::create_dir_all(&obs).unwrap();
        let active = obs.join("session_20260101_active.jsonl");
        let stale = obs.join("session_20260101_stale.jsonl");
        File::create(&active).unwrap();
        File::create(&stale).unwrap();

        let removed = prune_observation_jsonl(
            &projects,
            "20260501",
            Some(("20260101_active", "project-a")),
        )
        .unwrap();

        assert_eq!(removed, 1);
        assert!(active.exists());
        assert!(!stale.exists());
    }

    #[test]
    fn global_retention_cadence_allows_only_one_owner_per_interval() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let lease = try_acquire_global_retention_lease(&projects, SystemTime::now())
            .unwrap()
            .expect("first caller owns the lease");
        assert!(
            try_acquire_global_retention_lease(&projects, SystemTime::now())
                .unwrap()
                .is_none(),
            "a concurrent caller must not run a second global sweep"
        );
        drop(lease);
        record_global_retention(&projects).unwrap();
        assert!(
            try_acquire_global_retention_lease(&projects, SystemTime::now())
                .unwrap()
                .is_none(),
            "the cadence marker suppresses repeated global scans"
        );
    }

    #[test]
    fn retention_lease_blocks_concurrent_owner_and_is_reused_after_release() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let first = try_acquire_global_retention_lease(&projects, SystemTime::now())
            .unwrap()
            .expect("first caller owns the lease");
        assert!(
            try_acquire_global_retention_lease(&projects, SystemTime::now())
                .unwrap()
                .is_none(),
            "a concurrently held lease must not be acquired twice"
        );

        let lock = projects.join("retention.lock");
        assert!(lock.exists(), "the lease path is durable while held");
        drop(first);
        assert!(
            lock.exists(),
            "releasing a lease must not delete the persistent coordination path"
        );

        let successor = try_acquire_global_retention_lease(&projects, SystemTime::now())
            .unwrap()
            .expect("the released lease can be reused");
        assert!(lock.exists(), "the successor retains the coordination path");
        drop(successor);
        assert!(
            lock.exists(),
            "releasing the successor keeps the durable path"
        );
    }

    #[test]
    fn queued_sessions_protect_long_running_peer_projects() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let queue = projects.join("project-b").join("reflect-queue");
        fs::create_dir_all(&queue).unwrap();
        fs::write(
            queue.join("job_20260101_long.pending"),
            r#"{"session_id":"20260101_long","project":"project-b","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let active =
            active_reflection_sessions(&projects, Some(("20260101_ending", "project-a"))).unwrap();
        assert!(active.contains(&("20260101_ending".into(), "project-a".into())));
        assert!(active.contains(&("20260101_long".into(), "project-b".into())));
    }

    #[test]
    fn reflection_queue_rejects_more_than_the_entry_limit_without_pruning() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        let queue = project.join("reflect-queue");
        let obs = project.join("obs");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(&obs).unwrap();
        let retained = obs.join("session_20260101_active.jsonl");
        File::create(&retained).unwrap();
        for index in 0..=MAX_REFLECTION_QUEUE_ENTRIES {
            File::create(queue.join(format!("ignored-{index}"))).unwrap();
        }

        let error = active_reflection_sessions(&projects, None).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            retained.exists(),
            "a failed scan must run before any pruning"
        );
    }

    #[test]
    fn reflection_scan_rejects_more_than_the_project_limit() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        for index in 0..=MAX_RETENTION_PROJECTS {
            fs::create_dir_all(projects.join(format!("project-{index}"))).unwrap();
        }

        let error = active_reflection_sessions(&projects, None).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reflection_queue_rejects_more_than_the_valid_job_limit_without_pruning() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        let queue = project.join("reflect-queue");
        let obs = project.join("obs");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(&obs).unwrap();
        let retained = obs.join("session_20260101_active.jsonl");
        File::create(&retained).unwrap();
        for index in 0..=MAX_ACTIVE_REFLECTION_JOBS {
            fs::write(
                queue.join(format!("job-{index}.pending")),
                format!(r#"{{"session_id":"session-{index}"}}"#),
            )
            .unwrap();
        }

        let error = active_reflection_sessions(&projects, None).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            retained.exists(),
            "a failed scan must run before any pruning"
        );
    }

    #[test]
    fn reflection_queue_rejects_oversized_valid_job_without_pruning() {
        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        let queue = project.join("reflect-queue");
        let obs = project.join("obs");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(&obs).unwrap();
        let retained = obs.join("session_20260101_active.jsonl");
        File::create(&retained).unwrap();
        let oversized = queue.join("job_active.pending");
        fs::write(
            &oversized,
            format!(
                "{{\"session_id\":\"active\",\"padding\":\"{}\"}}",
                "x".repeat(MAX_REFLECTION_JOB_BYTES + 1)
            ),
        )
        .unwrap();

        let error = active_reflection_sessions(&projects, None).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            retained.exists(),
            "a failed scan must run before any pruning"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_marker_open_does_not_follow_a_leaf_swap_after_root_validation() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let external = dir.path().join("external");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir(&external).unwrap();
        let marker = projects.join("retention.last");
        let sentinel = external.join("retention.last");
        fs::write(&marker, "old").unwrap();
        fs::write(&sentinel, "sentinel").unwrap();

        let error = record_global_retention_after_root_open(&projects, || {
            fs::remove_file(&marker).unwrap();
            symlink(&sentinel, &marker).unwrap();
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::FilesystemLoop);
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn retention_marker_write_stays_in_open_root_after_parent_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let moved_projects = dir.path().join("projects-original");
        let external = dir.path().join("external");
        fs::create_dir(&projects).unwrap();
        fs::create_dir(&external).unwrap();
        let sentinel = external.join("retention.last");
        fs::write(&sentinel, "sentinel").unwrap();

        record_global_retention_after_root_open(&projects, || {
            fs::rename(&projects, &moved_projects).unwrap();
            symlink(&external, &projects).unwrap();
        })
        .unwrap();

        assert_eq!(fs::read_to_string(sentinel).unwrap(), "sentinel");
        assert!(moved_projects.join("retention.last").exists());
    }

    #[cfg(windows)]
    #[test]
    fn retention_rejects_a_junction_projects_root_when_creation_is_permitted() {
        use std::process::Command;

        let dir = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let external = dir.path().join("external");
        fs::create_dir(&external).unwrap();
        let projects_cmd = projects.to_string_lossy().replace("\\\\?\\", "");
        let external_cmd = external.to_string_lossy().replace("\\\\?\\", "");
        let command = format!("mklink /J {projects_cmd} {external_cmd}");
        if !Command::new("cmd")
            .args(["/C", command.as_str()])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let error = try_acquire_global_retention_lease(&projects, SystemTime::now())
            .expect_err("a reparse-point projects root must not be opened");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn retention_never_follows_a_project_obs_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        fs::create_dir_all(&project).unwrap();
        let external_file = external.path().join("session_20260101_external.jsonl");
        File::create(&external_file).unwrap();
        symlink(external.path(), project.join("obs")).unwrap();

        assert_eq!(
            prune_observation_jsonl(&projects, "20260501", None).unwrap(),
            0
        );
        assert!(external_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_never_follows_a_reflection_queue_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("project-a");
        fs::create_dir_all(&project).unwrap();
        let external_job = external.path().join("job_stale.completed");
        File::create(&external_job).unwrap();
        symlink(external.path(), project.join("reflect-queue")).unwrap();

        assert_eq!(
            prune_completed_reflection_jobs(
                &projects,
                Duration::from_secs(0),
                SystemTime::now() + Duration::from_secs(1),
            )
            .unwrap(),
            0
        );
        assert!(external_job.exists());
    }
}
