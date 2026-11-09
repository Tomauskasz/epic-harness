//! Eval commands run in disposable repositories/data roots; no user state is read.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    data: PathBuf,
}

#[test]
fn init_and_scaffold_json_modes_emit_one_valid_document() {
    let f = Fixture::new();
    let init = f.run("pass", &["eval", "--init", "--json"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&init.stdout).unwrap_or_else(|e| {
        panic!(
            "init invalid JSON: {e}: {}",
            String::from_utf8_lossy(&init.stdout)
        )
    });
    assert_eq!(value["action"], "init");
    assert!(PathBuf::from(value["path"].as_str().unwrap()).exists());
    fs::write(f.cwd.join("package.json"), "{}").unwrap();
    for _ in 0..2 {
        let scaffold = f.run("pass", &["eval", "--scaffold", "--json"]);
        assert!(scaffold.status.success());
        let value: serde_json::Value =
            serde_json::from_slice(&scaffold.stdout).unwrap_or_else(|e| {
                panic!(
                    "scaffold invalid JSON: {e}: {}",
                    String::from_utf8_lossy(&scaffold.stdout)
                )
            });
        assert_eq!(value["action"], "scaffold");
    }
}

#[test]
fn failed_report_and_baseline_writes_return_failure_without_false_confirmation() {
    for broken in ["results", "baselines", "latest"] {
        let f = Fixture::new();
        f.config(&format!("benchmarks:\n  - name: fixture\n    command: {}\ndimensions:\n  regression:\n    enabled: false\n", serde_json::to_string(&f.command()).unwrap()));
        match broken {
            "results" => {
                fs::create_dir_all(f.cwd.join("benchmarks")).unwrap();
                fs::write(f.cwd.join("benchmarks/results"), "not a directory").unwrap();
            }
            "baselines" => {
                fs::write(f.eval_dir().join("baselines"), "not a directory").unwrap();
            }
            _ => {
                fs::create_dir_all(f.eval_dir().join("baselines/latest.json")).unwrap();
            }
        }
        let output = f.run("pass", &["eval", "--baseline-update", "--json"]);
        assert!(!output.status.success(), "{broken} write falsely succeeded");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("baseline saved:"));
    }
}

#[test]
fn malformed_existing_baselines_do_not_silently_disable_regression() {
    let f = Fixture::new();
    f.config(&format!(
        "benchmarks:\n  - name: fixture\n    command: {}\n",
        serde_json::to_string(&f.command()).unwrap()
    ));
    assert!(
        f.run("pass", &["eval", "--json"]).status.success(),
        "missing first baseline is allowed"
    );
    let baselines = f.eval_dir().join("baselines");
    fs::create_dir_all(&baselines).unwrap();
    for invalid in [
        "{broken",
        "{}",
        r#"{"dimensions":{"benchmark:fixture":{"score":"not a number"}}}"#,
    ] {
        fs::write(baselines.join("latest.json"), invalid).unwrap();
        let output = f.run("pass", &["eval", "--json"]);
        assert!(!output.status.success(), "accepted baseline {invalid}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("baseline"));
    }
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let cwd = root.join("fixture-project");
        let data = root.join("data");
        for dir in [&cwd, &data, &root.join("home/.config/epic-harness")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            root.join("home/.config/epic-harness/telemetry-consent"),
            "off",
        )
        .unwrap();
        fs::write(
            data.join("config.toml"),
            "[dashboard]\nport=0\nauto_open=false\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            cwd,
            data,
        }
    }

    fn eval_dir(&self) -> PathBuf {
        self.data.join("projects/fixture-project/eval")
    }

    fn command(&self) -> String {
        format!(
            "\"{}\" --exact fixture_command --nocapture",
            std::env::current_exe().unwrap().display()
        )
    }

    fn config(&self, yaml: &str) {
        fs::create_dir_all(self.eval_dir()).unwrap();
        fs::write(self.eval_dir().join("eval.yaml"), yaml).unwrap();
    }

    fn run(&self, scenario: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_epic-harness"))
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("HARNESS_DIR", &self.data)
            .env("HARNESS_ROOT", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("EPIC_EVAL_TEST_SCENARIO", scenario)
            .env("PATH", "")
            .env_remove("HARNESS_DB_URL")
            .env_remove("HARNESS_MEM_URL")
            .env_remove("DATABASE_URL")
            .output()
            .unwrap()
    }
}

#[test]
fn native_timestamps_preserve_each_report_and_archived_baseline() {
    let f = Fixture::new();
    f.config(&format!("benchmarks:\n  - name: fixture\n    command: {}\ndimensions:\n  regression:\n    enabled: false\n", serde_json::to_string(&f.command()).unwrap()));
    let mut timestamps = Vec::new();
    for _ in 0..2 {
        let output = f.run("pass", &["eval", "--baseline-update", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let timestamp = report["timestamp"].as_str().unwrap().to_string();
        assert!(
            timestamp.starts_with("20"),
            "native UTC timestamp: {timestamp}"
        );
        timestamps.push(timestamp);
    }
    assert_ne!(timestamps[0], timestamps[1]);
    let reports = fs::read_dir(f.cwd.join("benchmarks/results"))
        .unwrap()
        .count();
    let baselines = fs::read_dir(f.eval_dir().join("baselines"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("BASELINE-"))
        .count();
    assert_eq!(reports, 2);
    assert_eq!(baselines, 2);
}

#[test]
fn fixture_command() {
    let Ok(scenario) = std::env::var("EPIC_EVAL_TEST_SCENARIO") else {
        return;
    };
    match scenario.as_str() {
        "passing-summary-failure" => {
            println!("test result: ok. 9 passed; 0 failed;");
            std::process::exit(7);
        }
        "partial-summary" => println!("test result: ok. 9 passed; 1 failed;"),
        "low-summary" => println!("test result: ok. 7 passed; 3 failed;"),
        "composite-low" => println!("{{\"composite\":0.1}}"),
        "composite-failure" => {
            println!("{{\"composite\":0.1}}");
            std::process::exit(7);
        }
        "composite-pretty" => println!("{{\n  \"composite\": 0.1\n}}"),
        "composite-missing" => println!("{{\"samples\":3}}"),
        "composite-out-of-range" => println!("{{\"composite\":2}}"),
        "marker" => {
            fs::write("EXECUTED.txt", "fixture command ran").unwrap();
            println!("test result: ok. 10 passed; 0 failed;");
        }
        "latency-fast" | "latency-slow" => {
            let started = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(
                if scenario == "latency-fast" { 5 } else { 250 },
            ));
            println!(
                "{}",
                serde_json::json!({"latency_ms": started.elapsed().as_secs_f64() * 1000.0})
            );
        }
        _ => println!("test result: ok. 10 passed; 0 failed;"),
    }
    std::process::exit(0);
}

#[test]
fn measured_performance_regressions_honor_the_configured_percentage() {
    let f = Fixture::new();
    f.config(&format!("dimensions:\n  performance:\n    enabled: true\n    command: {}\n    extra:\n      max_regression_pct: 20\n  regression:\n    enabled: true\n", serde_json::to_string(&f.command()).unwrap()));
    let baseline = f.run("latency-fast", &["eval", "--baseline-update", "--json"]);
    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let slow = f.run("latency-slow", &["eval", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&slow.stdout).unwrap();
    assert!(!slow.status.success(), "slow measurement passed: {report}");
    assert_eq!(
        report["dimensions"]["regression"]["details"]["deltas"]["performance"]["regressed"],
        true
    );
    assert_eq!(
        report["dimensions"]["performance"]["details"]["metric"],
        "latency_ms"
    );
}

#[test]
fn warning_only_regression_policy_does_not_hide_child_failures() {
    let f = Fixture::new();
    f.config(&format!("benchmarks:\n  - name: fixture\n    command: {}\n    result_type: composite\ndimensions:\n  regression:\n    enabled: true\n    extra:\n      fail_on_regression: false\n", serde_json::to_string(&f.command()).unwrap()));
    let baselines = f.eval_dir().join("baselines");
    fs::create_dir_all(&baselines).unwrap();
    fs::write(
        baselines.join("latest.json"),
        r#"{"dimensions":{"benchmark:fixture":{"score":0.9}}}"#,
    )
    .unwrap();
    let output = f.run("composite-low", &["eval", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(report["overall_verdict"], "WARN");
    assert_eq!(
        report["dimensions"]["regression"]["details"]["deltas"]["benchmark:fixture"]["regressed"],
        true
    );
    assert!(
        !f.run("composite-failure", &["eval", "--json"])
            .status
            .success()
    );
}

#[test]
fn help_is_read_only_and_invalid_eval_arguments_are_rejected() {
    let f = Fixture::new();
    f.config(&format!(
        "benchmarks:\n  - name: fixture\n    command: {}\n",
        serde_json::to_string(&f.command()).unwrap()
    ));
    let help = f.run("marker", &["eval", "--help", "--json"]);
    assert!(help.status.success());
    assert!(
        !f.cwd.join("EXECUTED.txt").exists(),
        "help executed the benchmark"
    );
    for args in [
        vec!["eval", "--dimension", "benchmakrs"],
        vec!["eval", "--dimension"],
        vec!["eval", "--dimension="],
        vec!["eval", "--bogus"],
    ] {
        assert!(
            !f.run("marker", &args).status.success(),
            "accepted invalid args {args:?}"
        );
    }
    assert!(!f.cwd.join("EXECUTED.txt").exists());
    assert!(f.run("marker", &["eval", "--json"]).status.success());
    assert!(
        f.cwd.join("EXECUTED.txt").exists(),
        "valid command control did not run"
    );
}

#[test]
fn composite_preserves_multiline_scores_and_rejects_missing_or_invalid_scores() {
    let f = Fixture::new();
    f.config(&format!("benchmarks:\n  - name: fixture\n    command: {}\n    result_type: composite\ndimensions:\n  regression:\n    enabled: false\n", serde_json::to_string(&f.command()).unwrap()));
    let pretty = f.run("composite-pretty", &["eval", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&pretty.stdout).unwrap();
    assert!(pretty.status.success());
    assert_eq!(report["dimensions"]["benchmark:fixture"]["score"], 0.1);
    for scenario in ["composite-missing", "composite-out-of-range"] {
        let output = f.run(scenario, &["eval", "--json"]);
        assert!(
            !output.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn regression_selection_runs_measurements_and_rejects_empty_comparisons() {
    let f = Fixture::new();
    f.config(&format!(
        "benchmarks:\n  - name: fixture\n    command: {}\n    result_type: composite\n",
        serde_json::to_string(&f.command()).unwrap()
    ));
    let baselines = f.eval_dir().join("baselines");
    fs::create_dir_all(&baselines).unwrap();
    fs::write(
        baselines.join("latest.json"),
        r#"{"dimensions":{"benchmark:fixture":{"score":0.9}}}"#,
    )
    .unwrap();
    let output = f.run(
        "composite-low",
        &["eval", "--dimension", "regression", "--json"],
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["dimensions"]["benchmark:fixture"]["score"], 0.1);
    assert_eq!(
        report["dimensions"]["regression"]["details"]["deltas"]["benchmark:fixture"]["regressed"],
        true
    );

    f.config("dimensions:\n  regression:\n    enabled: true\n");
    let empty = f.run("pass", &["eval", "--json"]);
    assert!(
        !empty.status.success(),
        "no measurement must not be a successful check"
    );
}

#[test]
fn correctness_nonzero_and_minimum_pass_rate_fail_the_gate() {
    let f = Fixture::new();
    f.config(&format!("stack: rust\ndimensions:\n  correctness:\n    enabled: true\n    command: {}\n    extra:\n      min_pass_rate: 1.0\n  regression:\n    enabled: false\n", serde_json::to_string(&f.command()).unwrap()));
    for scenario in ["passing-summary-failure", "partial-summary"] {
        let output = f.run(scenario, &["eval", "--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!output.status.success(), "scenario {scenario}: {report}");
        assert_eq!(report["overall_verdict"], "FAIL");
        assert_eq!(report["dimensions"]["correctness"]["passed"], false);
    }
    assert!(f.run("pass", &["eval", "--json"]).status.success());

    f.config(&format!("stack: rust\ndimensions:\n  correctness:\n    enabled: true\n    command: {}\n    extra:\n      min_pass_rate: 0.6\n  regression:\n    enabled: false\n", serde_json::to_string(&f.command()).unwrap()));
    let relaxed = f.run("low-summary", &["eval", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&relaxed.stdout).unwrap();
    assert!(relaxed.status.success(), "configured minimum met: {report}");
    assert_eq!(report["dimensions"]["correctness"]["passed"], true);
    assert_eq!(report["overall_verdict"], "WARN");
}
