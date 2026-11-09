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
use std::path::Path;
use std::str::FromStr;

use super::sqlx_err;
use crate::shared::paths::{global_harness_db_path, harness_projects_root};

#[cfg(test)]
#[path = "regression_254.rs"]
mod regression_254;

#[cfg(test)]
#[path = "regression_184.rs"]
mod regression_184;

// ── Public entry point ───────────────────────────────────────────────────────

pub fn run_merge(from: &str, to: &str, dry_run: bool, delete_source: bool) -> i32 {
    super::runtime::block_on(run_merge_async(from, to, dry_run, delete_source))
}

// ── Core async impl ──────────────────────────────────────────────────────────

async fn run_merge_async(from: &str, to: &str, dry_run: bool, delete_source: bool) -> i32 {
    let projects_root = harness_projects_root();
    let from_dir = projects_root.join(from);
    let to_dir = projects_root.join(to);

    if !from_dir.is_dir() {
        eprintln!("[merge-project] source slug not found: {from}");
        eprintln!("  expected: {}", from_dir.display());
        return 1;
    }
    if from == to {
        eprintln!("[merge-project] --from and --to must be different slugs");
        return 1;
    }

    println!("[merge-project] from: {from}  →  to: {to}");
    if dry_run {
        println!("[merge-project] DRY RUN — no changes will be made\n");
    }

    if !to_dir.is_dir() {
        if dry_run {
            println!("  [dry-run] would create {}", to_dir.display());
        } else if let Err(e) = std::fs::create_dir_all(&to_dir) {
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
    let obs_files = merge_file_count(
        copy_dir_files(&from_dir.join("obs"), &to_dir.join("obs"), dry_run),
        "observations",
        &mut exit,
    );
    let session_files = merge_file_count(
        copy_dir_files(
            &from_dir.join("sessions"),
            &to_dir.join("sessions"),
            dry_run,
        ),
        "sessions",
        &mut exit,
    );
    let orbit_files = merge_file_count(
        copy_dir_files(&from_dir.join("orbit"), &to_dir.join("orbit"), dry_run),
        "orbit",
        &mut exit,
    );
    let evolved_dirs = merge_file_count(
        copy_evolved_dir(&from_dir.join("evolved"), &to_dir.join("evolved"), dry_run),
        "evolved skills",
        &mut exit,
    );
    let evo_lines = merge_file_count(
        append_evolution_jsonl(&from_dir, &to_dir, dry_run),
        "evolution history",
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

    if delete_source && exit != 0 {
        eprintln!("[merge-project] source retained because at least one transfer failed");
    } else if delete_source {
        if dry_run {
            println!("  [dry-run] would delete   : {}", from_dir.display());
        } else {
            match std::fs::remove_dir_all(&from_dir) {
                Ok(_) => println!("  source dir deleted       : {}", from_dir.display()),
                Err(e) => {
                    eprintln!("[merge-project] could not delete source: {e}");
                    exit = 1;
                }
            }
        }
    }

    println!("─────────────────────────────────────────────────────────");
    if dry_run {
        println!("[merge-project] dry run complete — omit --dry-run to apply");
    } else {
        println!("[merge-project] done");
    }

    exit
}

fn merge_file_count(result: io::Result<usize>, label: &str, exit: &mut i32) -> usize {
    match result {
        Ok(count) => count,
        Err(error) => {
            eprintln!("[merge-project] {label} transfer failed: {error}");
            *exit = 1;
            0
        }
    }
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
            Err(e) if e.to_string().contains("no such") => {}
            Err(e) => eprintln!("[merge-project] update {table}: {e}"),
        }
    }

    // metrics_state: composite PK (key, project) — drop conflicting source rows, then rename.
    let _ = sqlx::query(
        "DELETE FROM metrics_state WHERE project = ? \
         AND key IN (SELECT key FROM metrics_state WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut conn)
    .await;
    if let Ok(r) = sqlx::query("UPDATE metrics_state SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        total += r.rows_affected() as usize;
    }

    // skill_attribution: composite PK (skill_name, project) — keep target rows on conflict.
    let _ = sqlx::query(
        "DELETE FROM skill_attribution WHERE project = ? \
         AND skill_name IN (SELECT skill_name FROM skill_attribution WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut conn)
    .await;
    if let Ok(r) = sqlx::query("UPDATE skill_attribution SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        total += r.rows_affected() as usize;
    }

    // promotion_counters: composite PK (pattern_key, project) — sum counts on conflict.
    let _ = sqlx::query(
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
    .await;
    let _ = sqlx::query(
        "DELETE FROM promotion_counters WHERE project = ? \
         AND pattern_key IN (SELECT pattern_key FROM promotion_counters WHERE project = ?)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut conn)
    .await;
    if let Ok(r) = sqlx::query("UPDATE promotion_counters SET project = ? WHERE project = ?")
        .bind(to)
        .bind(from)
        .execute(&mut conn)
        .await
    {
        total += r.rows_affected() as usize;
    }

    conn.execute("COMMIT").await.map_err(sqlx_err)?;
    Ok(total)
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
    if !src_dir.exists() {
        return Ok(0);
    }
    let entries = std::fs::read_dir(src_dir)?;
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let name = entry.file_name();
        let dst = dst_dir.join(&name);
        if dst.exists() {
            if std::fs::read(&src)? == std::fs::read(&dst)? {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("distinct source file conflicts with {}", dst.display()),
            ));
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
    if !src_evolved.exists() {
        return Ok(0);
    }
    let entries = std::fs::read_dir(src_evolved)?;
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let src = entry.path();
        if !src.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let dst = dst_evolved.join(&name);
        if dst.exists() {
            if same_directory_contents(&src, &dst)? {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("distinct source skill conflicts with {}", dst.display()),
            ));
        }
        if !dry_run {
            copy_dir_recursive(&src, &dst)?;
        }
        count += 1;
    }
    Ok(count)
}

fn same_directory_contents(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left_entries = std::fs::read_dir(left)?.collect::<io::Result<Vec<_>>>()?;
    let mut right_entries = std::fs::read_dir(right)?.collect::<io::Result<Vec<_>>>()?;
    left_entries.sort_by_key(|entry| entry.file_name());
    right_entries.sort_by_key(|entry| entry.file_name());
    if left_entries.len() != right_entries.len() {
        return Ok(false);
    }
    for (a, b) in left_entries.iter().zip(&right_entries) {
        if a.file_name() != b.file_name() {
            return Ok(false);
        }
        let (a_type, b_type) = (a.file_type()?, b.file_type()?);
        if a_type.is_symlink() || b_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "links in evolved skills require explicit resolution",
            ));
        }
        if a_type.is_dir() && b_type.is_dir() {
            if !same_directory_contents(&a.path(), &b.path())? {
                return Ok(false);
            }
        } else if a_type.is_file() && b_type.is_file() {
            if std::fs::read(a.path())? != std::fs::read(b.path())? {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
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

    // Only complete equal records are duplicates. Shared metadata prefixes are common.
    let mut existing: std::collections::HashSet<String> = if dst.exists() {
        std::fs::read_to_string(&dst)?
            .lines()
            .map(String::from)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let new_lines: Vec<&str> = src_lines
        .iter()
        .filter(|l| existing.insert((**l).to_string()))
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
    use sqlx::Connection;

    async fn test_db(path: &Path) -> sqlx::SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::raw_sql(crate::store::schema::DDL_SQLITE)
            .execute(&mut conn)
            .await
            .unwrap();
        conn
    }

    #[tokio::test]
    async fn issue_183_failed_database_merge_retains_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let home = temp.path().join("home");
        let source = root.join("projects/source");
        let target = root.join("projects/target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(target.join("harness.db")).unwrap();
        std::fs::create_dir_all(home.join(".config/epic-harness")).unwrap();
        std::fs::write(home.join(".config/epic-harness/telemetry-consent"), "off").unwrap();
        std::fs::write(
            root.join("config.toml"),
            "[dashboard]\nport=0\nauto_open=false\n",
        )
        .unwrap();
        let mut conn = test_db(&source.join("harness.db")).await;
        sqlx::query("INSERT INTO sessions(timestamp, summary, created_at_millis, project) VALUES('2026-01-01','retained sentinel',1,'source')")
            .execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::merge_project::tests::issue_183_child_merge",
            ])
            .current_dir(&source)
            .env("EPIC_STORAGE_CHILD_CASE", "183")
            .env("HARNESS_DIR", &root)
            .env("HARNESS_ROOT", temp.path())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env_remove("HARNESS_DB_URL")
            .env_remove("HARNESS_MEM_URL")
            .env_remove("DATABASE_URL")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            source.join("harness.db").is_file(),
            "a failed transfer must preserve its only source"
        );
        let mut conn = test_db(&source.join("harness.db")).await;
        let summary: String = sqlx::query_scalar("SELECT summary FROM sessions")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(summary, "retained sentinel");
    }

    #[test]
    fn issue_183_child_merge() {
        if std::env::var("EPIC_STORAGE_CHILD_CASE").as_deref() == Ok("183") {
            assert_eq!(run_merge("source", "target", false, true), 1);
        }
    }

    #[test]
    fn issue_255_preserves_full_records_and_deduplicates_retries() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let prefix = "x".repeat(100);
        // Keep the differing field after a shared prefix, as ordinary records do.
        let first = format!("{{\"prefix\":\"{prefix}\",\"analysis_summary\":\"first\"}}");
        let second = format!("{{\"prefix\":\"{prefix}\",\"analysis_summary\":\"second\"}}");
        assert_ne!(first, second);
        std::fs::write(target.join("evolution.jsonl"), format!("{first}\n")).unwrap();
        std::fs::write(
            source.join("evolution.jsonl"),
            format!("{second}\n{second}\n"),
        )
        .unwrap();
        assert_eq!(append_evolution_jsonl(&source, &target, false).unwrap(), 1);
        assert_eq!(append_evolution_jsonl(&source, &target, false).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(target.join("evolution.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}
