use super::*;
use sqlx::Connection;

async fn database(path: &std::path::Path) -> sqlx::SqliteConnection {
    let mut c = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(crate::store::schema::DDL_SQLITE)
        .execute(&mut c)
        .await
        .unwrap();
    c
}

#[tokio::test]
async fn issue_185_copies_all_operational_tables_and_scalar_fields() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.db");
    let mut s = database(&source).await;
    sqlx::raw_sql("INSERT INTO sessions(timestamp,summary,created_at_millis) VALUES('2026-11-09','control',1);
      INSERT INTO evolution_records(timestamp,edit_type) VALUES('2026-11-09','modify_skill');
      INSERT INTO skill_attribution(skill_name,first_seen,sessions_holdout) VALUES('synthetic','2026-11-09',6);
      INSERT INTO orbit_pipelines(id,project,state_json,created_at,updated_at) VALUES('pipeline','old','{\"value\":7}','2026-11-09','2026-11-09');
      INSERT INTO evolved_skills(name,project,skill_md,created,updated) VALUES('synthetic','old','preserved guidance','2026-11-09','2026-11-09');
      INSERT INTO global_patterns(timestamp,project,weak_tools) VALUES('2026-11-09','old','[\"synthetic\"]');
      INSERT INTO workspace_manifest(id,updated,skills_json) VALUES(1,'2026-11-09','[\"synthetic\"]');")
      .execute(&mut s).await.unwrap();
    s.close().await.unwrap();
    let mut target = database(&temp.path().join("target.db")).await;
    sqlx::query("ATTACH DATABASE ? AS src")
        .bind(source.to_string_lossy().as_ref())
        .execute(&mut target)
        .await
        .unwrap();
    merge_attached_db_async(&mut target, "target", "src")
        .await
        .unwrap();
    let pipeline: String =
        sqlx::query_scalar("SELECT state_json FROM orbit_pipelines WHERE project='target'")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert_eq!(pipeline, "{\"value\":7}");
    let skill: String =
        sqlx::query_scalar("SELECT skill_md FROM evolved_skills WHERE project='target'")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert_eq!(skill, "preserved guidance");
    let global: String =
        sqlx::query_scalar("SELECT weak_tools FROM global_patterns WHERE project='target'")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert_eq!(global, "[\"synthetic\"]");
    let workspace: String = sqlx::query_scalar("SELECT skills_json FROM workspace_manifest")
        .fetch_one(&mut target)
        .await
        .unwrap();
    assert_eq!(workspace, "[\"synthetic\"]");
    let edit: String =
        sqlx::query_scalar("SELECT edit_type FROM evolution_records WHERE project='target'")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert_eq!(edit, "modify_skill");
    let holdout: i64 =
        sqlx::query_scalar("SELECT sessions_holdout FROM skill_attribution WHERE project='target'")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert_eq!(holdout, 6);
}

#[tokio::test]
async fn issue_185_copy_error_rolls_back_earlier_tables() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.db");
    let mut s = database(&source).await;
    sqlx::raw_sql("INSERT INTO sessions(timestamp,summary,created_at_millis) VALUES('2026-11-09','must roll back',1); DROP TABLE evolution_records; CREATE TABLE evolution_records(analysis_summary TEXT); INSERT INTO evolution_records VALUES('untransferable record');").execute(&mut s).await.unwrap();
    s.close().await.unwrap();
    let mut target = database(&temp.path().join("target.db")).await;
    sqlx::query("ATTACH DATABASE ? AS src")
        .bind(source.to_string_lossy().as_ref())
        .execute(&mut target)
        .await
        .unwrap();
    assert!(
        merge_attached_db_async(&mut target, "target", "src")
            .await
            .is_err(),
        "unsupported source shape must not report success"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&mut target)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn issue_185_legacy_optional_columns_keep_documented_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.db");
    let mut s = database(&source).await;
    sqlx::raw_sql("ALTER TABLE evolution_records DROP COLUMN edit_type; ALTER TABLE skill_attribution DROP COLUMN sessions_holdout; INSERT INTO evolution_records(timestamp) VALUES('2026-11-09'); INSERT INTO skill_attribution(skill_name,first_seen) VALUES('legacy','2026-11-09');").execute(&mut s).await.unwrap();
    s.close().await.unwrap();
    let mut target = database(&temp.path().join("target.db")).await;
    sqlx::query("ATTACH DATABASE ? AS src")
        .bind(source.to_string_lossy().as_ref())
        .execute(&mut target)
        .await
        .unwrap();
    merge_attached_db_async(&mut target, "target", "src")
        .await
        .unwrap();
    let edit: String = sqlx::query_scalar("SELECT edit_type FROM evolution_records")
        .fetch_one(&mut target)
        .await
        .unwrap();
    assert_eq!(edit, "add_skill");
    let holdout: i64 = sqlx::query_scalar("SELECT sessions_holdout FROM skill_attribution")
        .fetch_one(&mut target)
        .await
        .unwrap();
    assert_eq!(holdout, 0);
}
