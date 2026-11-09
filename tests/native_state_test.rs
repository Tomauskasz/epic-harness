//! Exercise the actual Tauri state constructor without a WebView or Tauri runtime.
#[path = "../src-tauri/src/state.rs"]
mod native_state;

#[test]
fn native_pools_work_after_the_initialization_runtime_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    // This integration binary contains one test. Set isolation before starting
    // any Tokio threads or initializing the process-wide config/pool caches.
    unsafe {
        std::env::set_var("HARNESS_DIR", root.path());
        std::env::remove_var("HARNESS_ROOT");
        std::env::remove_var("HARNESS_DB_URL");
        std::env::remove_var("HARNESS_MEM_URL");
    }
    std::fs::write(root.path().join("config.toml"), "").unwrap();
    let state = native_state::AppState::new().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for (name, pool, query) in [
            ("memory", &state.db, "SELECT COUNT(*) FROM nodes"),
            (
                "harness",
                &state.harness_db,
                "SELECT COUNT(*) FROM observations",
            ),
        ] {
            let count: (i64,) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sqlx::query_as(query).fetch_one(pool),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("{name} pool timed out after initialization runtime dropped")
            })
            .unwrap_or_else(|e| {
                panic!("{name} pool failed after initialization runtime dropped: {e}")
            });
            assert_eq!(count.0, 0, "fresh {name} fixture must be empty");
        }
        epic_harness::store::pool::shutdown().await;
    });
}
