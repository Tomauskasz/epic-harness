//! CLI entrypoint for Orbit lifecycle commands.

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
        Some("complete") => {
            let _ = harness_dir;
            eprintln!(
                "error: Orbit completion is recorded only by the durable SessionEnd reflection worker"
            );
            1
        }
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
    fn complete_command_rejects_pipeline_without_session_end_evolution_evidence() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        let pipeline = orbit.join("PIPELINE-20260729-cli.json");
        fs::write(
            &pipeline,
            r#"{"id":"cli","status":"running","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#,
        )
        .unwrap();

        let before = fs::read_to_string(&pipeline).unwrap();

        assert_eq!(run_in(&["complete".to_string()], harness.path()), 1);
        assert_eq!(fs::read_to_string(pipeline).unwrap(), before);
    }
}
