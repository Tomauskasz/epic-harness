//! merge_project.rs — Consolidate two project slugs into one.
//!
//! `epic-harness merge-project --from <slug> --to <slug> [--dry-run] [--delete-source]`
//!
//! Three-layer merge:
//! 1. Global harness.db (`~/.harness/harness.db`): UPDATE project column from→to.
//! 2. Per-project harness.db: ATTACH source DB, INSERT OR IGNORE into target.
//! 3. File-based: obs/*.jsonl, sessions/*.json, evolved/, evolution.jsonl, orbit/.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Executor};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use super::sqlx_err;
use crate::shared::paths::{canonical_for_compare, global_harness_db_path, harness_projects_root};

// ── Public entry point ───────────────────────────────────────────────────────

pub fn run_merge(from: &str, to: &str, dry_run: bool, delete_source: bool) -> i32 {
    super::runtime::block_on(run_merge_async(from, to, dry_run, delete_source))
}

// ── Core async impl ──────────────────────────────────────────────────────────

async fn run_merge_async(from: &str, to: &str, dry_run: bool, delete_source: bool) -> i32 {
    let projects_root = harness_projects_root();
    let from_dir = match resolve_merge_project_dir(&projects_root, from, true) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[merge-project] invalid source: {error}");
            return 1;
        }
    };
    let to_dir = match resolve_merge_project_dir(&projects_root, to, false) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[merge-project] invalid target: {error}");
            return 1;
        }
    };
    if from == to {
        eprintln!("[merge-project] --from and --to must be different slugs");
        return 1;
    }
    if paths_equal(&from_dir, &to_dir) {
        eprintln!("[merge-project] source and target resolve to the same directory");
        return 1;
    }

    println!("[merge-project] from: {from}  →  to: {to}");
    if dry_run {
        println!("[merge-project] DRY RUN — no changes will be made\n");
    }

    if !to_dir.is_dir() {
        if dry_run {
            println!("  [dry-run] would create {}", to_dir.display());
        } else if let Err(e) = std::fs::create_dir(&to_dir) {
            eprintln!("[merge-project] cannot create target dir: {e}");
            return 1;
        }
    }

    let mut exit = 0;
    let mut global_rows = 0usize;
    let mut db_obs = 0usize;
    let mut db_sessions = 0usize;
    let mut db_evo = 0usize;

    // ── 1. Global harness.db ─────────────────────────────────────────────────
    let global_db = global_harness_db_path();
    if global_db.exists() {
        match merge_global_db(&global_db, from, to, dry_run).await {
            Ok(n) => global_rows = n,
            Err(e) => {
                eprintln!("[merge-project] global DB error: {e}");
                exit = 1;
            }
        }
    }

    // ── 2. Per-project harness.db ────────────────────────────────────────────
    let from_db = from_dir.join("harness.db");
    let to_db = to_dir.join("harness.db");
    if from_db.exists() {
        match merge_per_project_dbs(&from_db, &to_db, to, dry_run).await {
            Ok((o, s, e)) => {
                db_obs = o;
                db_sessions = s;
                db_evo = e;
            }
            Err(e) => {
                eprintln!("[merge-project] per-project DB error: {e}");
                exit = 1;
            }
        }
    }

    // ── 3. File-based data ───────────────────────────────────────────────────
    let obs_files = merge_file_result(
        "obs files",
        copy_dir_files(&from_dir.join("obs"), &to_dir.join("obs"), dry_run),
        &mut exit,
    );
    let session_files = merge_file_result(
        "session files",
        copy_dir_files(
            &from_dir.join("sessions"),
            &to_dir.join("sessions"),
            dry_run,
        ),
        &mut exit,
    );
    let orbit_files = merge_file_result(
        "orbit files",
        copy_dir_files(&from_dir.join("orbit"), &to_dir.join("orbit"), dry_run),
        &mut exit,
    );
    let evolved_dirs = merge_file_result(
        "evolved directories",
        copy_evolved_dir(&from_dir.join("evolved"), &to_dir.join("evolved"), dry_run),
        &mut exit,
    );
    let evo_lines = merge_file_result(
        "evolution.jsonl",
        append_evolution_jsonl(&from_dir, &to_dir, dry_run),
        &mut exit,
    );

    // ── Summary ──────────────────────────────────────────────────────────────
    println!("\n[merge-project] ─── summary ───────────────────────────");
    println!("  global DB rows updated   : {global_rows}");
    println!("  per-project DB merged    : obs={db_obs} sessions={db_sessions} evo={db_evo}");
    println!("  obs files copied         : {obs_files}");
    println!("  session files copied     : {session_files}");
    println!("  evolved dirs copied      : {evolved_dirs}");
    println!("  evolution.jsonl lines    : {evo_lines}");
    println!("  orbit files copied       : {orbit_files}");

    if delete_source && exit == 0 {
        if dry_run {
            println!("  [dry-run] would delete   : {}", from_dir.display());
        } else {
            match delete_validated_source(&from_dir, &projects_root) {
                Ok(_) => println!("  source dir deleted       : {}", from_dir.display()),
                Err(e) => {
                    eprintln!("[merge-project] could not delete source: {e}");
                    exit = 1;
                }
            }
        }
    } else if delete_source {
        eprintln!("[merge-project] source retained because merge reported errors");
    }

    println!("─────────────────────────────────────────────────────────");
    if dry_run {
        println!("[merge-project] dry run complete — omit --dry-run to apply");
    } else {
        println!("[merge-project] done");
    }

    exit
}

fn merge_file_result(label: &str, result: io::Result<usize>, exit: &mut i32) -> usize {
    match result {
        Ok(count) => count,
        Err(error) => {
            eprintln!("[merge-project] {label} error: {error}");
            *exit = 1;
            0
        }
    }
}

/// Resolve a user-provided project slug to a direct child of `projects_root`.
/// This boundary intentionally accepts one normal path component only: a merge
/// must never turn a CLI value into an arbitrary directory or delete target.
fn resolve_merge_project_dir(
    projects_root: &Path,
    slug: &str,
    require_existing: bool,
) -> io::Result<PathBuf> {
    validate_merge_slug(slug)?;
    let root = canonical_for_compare(projects_root)?;
    let candidate = projects_root.join(slug);

    if require_existing {
        let metadata = candidate.symlink_metadata()?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "source is not a regular project directory: {}",
                    candidate.display()
                ),
            ));
        }
    } else if candidate.exists() {
        let metadata = candidate.symlink_metadata()?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "target is not a regular project directory: {}",
                    candidate.display()
                ),
            ));
        }
    }

    let resolved = if candidate.exists() {
        canonical_for_compare(&candidate)?
    } else {
        root.join(slug)
    };
    if paths_equal(&resolved, &root) || !resolved.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("project escapes projects root: {}", candidate.display()),
        ));
    }
    Ok(resolved)
}

fn validate_merge_slug(slug: &str) -> io::Result<()> {
    let mut components = Path::new(slug).components();
    let valid_component = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !slug.contains(':');
    if valid_component {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected one project slug component, got {slug:?}"),
        ))
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        return left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy());
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn delete_validated_source(source: &Path, projects_root: &Path) -> io::Result<()> {
    let root = canonical_for_compare(projects_root)?;
    let current = canonical_for_compare(source)?;
    if paths_equal(&current, &root) || !current.starts_with(&root) || !paths_equal(&current, source)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "source changed or escapes projects root: {}",
                source.display()
            ),
        ));
    }
    std::fs::remove_dir_all(&current)
}

// ── Global DB: UPDATE project column ────────────────────────────────────────

async fn merge_global_db(db_path: &Path, from: &str, to: &str, dry_run: bool) -> io::Result<usize> {
    let url = format!("sqlite:{}", db_path.display());
    let mut conn = SqliteConnectOptions::from_str(&url)
        .map_err(sqlx_err)?
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await
        .map_err(sqlx_err)?;

    // Tables where project is a plain (non-PK) column — safe to UPDATE directly.
    const SIMPLE: &[&str] = &[
        "observations",
        "sessions",
        "evolution_records",
        "score_history",
        "orch_runs",
        "orch_control",
        "orbit_pipelines",
        "evolved_skills",
        "global_patterns",
    ];

    let mut total = 0usize;

    if dry_run {
        for table in SIMPLE {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE project = ?"
            )))
            .bind(from)
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
            if n > 0 {
                println!("  [dry-run] {table}: {n} rows would be re-labelled");
                total += n as usize;
            }
        }
        for table in &["metrics_state", "skill_attribution", "promotion_counters"] {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE project = ?"
            )))
            .bind(from)
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
            if n > 0 {
                println!("  [dry-run] {table}: {n} rows would be merged/re-labelled");
                total += n as usize;
            }
        }
        return Ok(total);
    }

    conn.execute("BEGIN IMMEDIATE").await.map_err(sqlx_err)?;

    for table in SIMPLE {
        match sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET project = ? WHERE project = ?"
        )))
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
        {
            Ok(r) => total += r.rows_affected() as usize,
            Err(e) if is_missing_table(&e) => {}
            Err(error) => return Err(sqlx_err(error)),
        }
    }

    // metrics_state: composite PK (key, project) — drop conflicting source rows, then rename.
    match sqlx::query(
        "DELETE FROM metrics_state WHERE project = ? \
         AND key IN (SELECT key FROM metrics_state WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut conn)
    .await
    {
        Ok(_) => {}
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }
    match sqlx::query("UPDATE metrics_state SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        Ok(result) => total += result.rows_affected() as usize,
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }

    // skill_attribution: composite PK (skill_name, project) — keep target rows on conflict.
    match sqlx::query(
        "DELETE FROM skill_attribution WHERE project = ? \
         AND skill_name IN (SELECT skill_name FROM skill_attribution WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut conn)
    .await
    {
        Ok(_) => {}
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }
    match sqlx::query("UPDATE skill_attribution SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        Ok(result) => total += result.rows_affected() as usize,
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }

    // promotion_counters: composite PK (pattern_key, project) — sum counts on conflict.
    match sqlx::query(
        "UPDATE promotion_counters \
         SET count = count + (
             SELECT count FROM promotion_counters AS src
             WHERE src.project = ? AND src.pattern_key = promotion_counters.pattern_key
         ) \
         WHERE project = ? \
         AND pattern_key IN (SELECT pattern_key FROM promotion_counters WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .bind(from)
    .execute(&mut conn)
    .await
    {
        Ok(_) => {}
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }
    match sqlx::query("DELETE FROM promotion_counters WHERE project = ?")
        .bind(from)
        .execute(&mut conn)
        .await
    {
        Ok(_) => {}
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }
    match sqlx::query("UPDATE promotion_counters SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        Ok(result) => total += result.rows_affected() as usize,
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(sqlx_err(error)),
    }

    conn.execute("COMMIT").await.map_err(sqlx_err)?;
    Ok(total)
}

fn is_missing_table(error: &sqlx::Error) -> bool {
    error.to_string().contains("no such table")
}

// ── Per-project harness.db: ATTACH + INSERT OR IGNORE ───────────────────────

async fn merge_per_project_dbs(
    from_db: &Path,
    to_db: &Path,
    to_slug: &str,
    dry_run: bool,
) -> io::Result<(usize, usize, usize)> {
    if dry_run {
        let url = format!("sqlite:{}", from_db.display());
        let mut conn = SqliteConnectOptions::from_str(&url)
            .map_err(sqlx_err)?
            .journal_mode(SqliteJournalMode::Wal)
            .read_only(true)
            .connect()
            .await
            .map_err(sqlx_err)?;
        let obs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
        let sess: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
        let evo: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM evolution_records")
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
        println!("  [dry-run] per-project DB would merge: obs={obs} sessions={sess} evo={evo}");
        return Ok((obs as usize, sess as usize, evo as usize));
    }

    if let Some(parent) = to_db.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let url = format!("sqlite:{}", to_db.display());
    let mut conn = SqliteConnectOptions::from_str(&url)
        .map_err(sqlx_err)?
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true)
        .connect()
        .await
        .map_err(sqlx_err)?;

    // Init schema if this is a brand-new target DB.
    let table_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")
            .fetch_one(&mut conn)
            .await
            .unwrap_or(0);
    if table_count == 0 {
        conn.execute(super::schema::DDL_SQLITE)
            .await
            .map_err(sqlx_err)?;
    }

    let escaped = from_db.to_string_lossy().replace('\'', "''");
    conn.execute(sqlx::AssertSqlSafe(format!(
        "ATTACH DATABASE '{escaped}' AS src"
    )))
    .await
    .map_err(sqlx_err)?;

    let stats = super::migrate::merge_attached_db_async(&mut conn, to_slug, "src").await?;

    conn.execute("DETACH DATABASE src")
        .await
        .map_err(sqlx_err)?;

    Ok((stats.obs, stats.sessions, stats.evo))
}

// ── File-level helpers ───────────────────────────────────────────────────────

/// Copy files from `src_dir` to `dst_dir`, skipping files that already exist.
/// Returns the number of files copied (or would copy in dry-run).
fn copy_dir_files(src_dir: &Path, dst_dir: &Path, dry_run: bool) -> io::Result<usize> {
    let entries = match std::fs::read_dir(src_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let src = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let dst = dst_dir.join(&name);
        if dst.exists() {
            continue;
        }
        if dry_run {
            count += 1;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Copy evolved skill subdirectories that don't already exist in target.
fn copy_evolved_dir(src_evolved: &Path, dst_evolved: &Path, dry_run: bool) -> io::Result<usize> {
    let entries = match std::fs::read_dir(src_evolved) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let src = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let dst = dst_evolved.join(&name);
        if dst.exists() {
            continue;
        }
        if !dry_run {
            copy_dir_recursive(&src, &dst)?;
        }
        count += 1;
    }
    Ok(count)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// Append lines from source evolution.jsonl that don't exist in target (dedup by content hash).
fn append_evolution_jsonl(from_dir: &Path, to_dir: &Path, dry_run: bool) -> io::Result<usize> {
    let src = from_dir.join("evolution.jsonl");
    let dst = to_dir.join("evolution.jsonl");
    if !src.exists() {
        return Ok(0);
    }

    let src_lines = std::fs::read_to_string(&src)?;
    let src_lines: Vec<&str> = src_lines.lines().filter(|l| !l.trim().is_empty()).collect();
    if src_lines.is_empty() {
        return Ok(0);
    }

    // Build set of existing lines (by first 80 chars as a cheap dedup key).
    let existing: std::collections::HashSet<String> = if dst.exists() {
        std::fs::read_to_string(&dst)?
            .lines()
            .map(|l| l.chars().take(80).collect())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let new_lines: Vec<&str> = src_lines
        .iter()
        .filter(|l| {
            let key: String = l.chars().take(80).collect();
            !existing.contains(&key)
        })
        .copied()
        .collect();

    if new_lines.is_empty() {
        return Ok(0);
    }

    if !dry_run {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&dst)?;
        for line in &new_lines {
            writeln!(f, "{line}")?;
        }
    }

    Ok(new_lines.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn merge_rejects_non_slug_paths_before_resolving_directories() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        std::fs::create_dir_all(projects.join("safe")).unwrap();

        for require_existing in [true, false] {
            for slug in [
                "",
                ".",
                "..",
                "safe/child",
                r"C:\\",
                root.path().to_str().unwrap(),
            ] {
                assert!(
                    resolve_merge_project_dir(&projects, slug, require_existing).is_err(),
                    "unsafe merge slug must be rejected: {slug:?}"
                );
            }
        }
    }

    #[test]
    fn merge_resolves_an_existing_direct_project_child() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        let source = projects.join("source");
        std::fs::create_dir_all(&source).unwrap();

        assert_eq!(
            resolve_merge_project_dir(&projects, "source", true).unwrap(),
            canonical_for_compare(&source).unwrap()
        );
    }

    #[test]
    fn delete_validated_source_removes_only_a_direct_project_child() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        let source = projects.join("source");
        std::fs::create_dir_all(&source).unwrap();

        let resolved = resolve_merge_project_dir(&projects, "source", true).unwrap();
        delete_validated_source(&resolved, &projects).unwrap();
        assert!(!source.exists());
        assert!(projects.is_dir());
    }

    #[test]
    #[serial]
    fn merge_error_retains_source_when_delete_source_was_requested() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join(".harness/projects/source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            home.path().join(".harness/harness.db"),
            "not a sqlite database",
        )
        .unwrap();

        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let exit = run_merge("source", "target", false, true);
        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_ne!(exit, 0);
        assert!(source.is_dir(), "source must survive any merge error");
    }
}
