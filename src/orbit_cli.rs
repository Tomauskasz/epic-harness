//! CLI entrypoint for validated Orbit pipeline completion.
//!
//! Usage: `epic orbit complete`. The command locates the active pipeline and
//! delegates its validation and atomic completion transition to `shared::orbit`.

use std::path::Path;

/// Run the `epic orbit` subcommand and return its process exit code.
pub fn run(args: &[String]) -> i32 {
    let harness_dir = std::env::var_os("HARNESS_DIR")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::hooks::common::harness_dir);
    run_in(args, &harness_dir)
}

fn run_in(args: &[String], harness_dir: &Path) -> i32 {
    match args.first().map(String::as_str) {
        Some("complete") => match crate::shared::orbit::complete_pipeline_in(harness_dir) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        _ => {
            eprintln!("Usage: epic orbit complete");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn complete_command_routes_to_the_atomic_transition() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-cli.json");
        fs::write(
            &pipeline,
            r#"{"id":"cli","status":"running","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#,
        )
        .unwrap();

        assert_eq!(run_in(&["complete".to_string()], harness.path()), 0);
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pipeline).unwrap()).unwrap();
        assert_eq!(state["status"], "complete");
    }
}
