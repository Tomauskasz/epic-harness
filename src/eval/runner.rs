//! eval/runner.rs — Execute eval commands and parse results

use std::time::Instant;

use serde_json::{Value, json};

use super::config::{Benchmark, ResolvedCommands};

/// Single dimension evaluation result.
#[derive(Debug, Clone)]
pub struct DimResult {
    pub dimension: String,
    pub score: f64,
    pub passed: bool,
    pub verdict: String, // PASS, WARN, FAIL, SKIPPED
    pub details: Value,
    pub duration_ms: u64,
}

// ── Correctness ─────────────────────────────────────────────────────

pub fn run_correctness(cmds: &ResolvedCommands) -> DimResult {
    let start = Instant::now();

    let test_cmd = match &cmds.test_command {
        Some(c) => c,
        None => {
            return DimResult {
                dimension: "correctness".into(),
                score: 0.0,
                passed: false,
                verdict: "SKIPPED".into(),
                details: json!({"reason": "no test command configured or detected"}),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let output = exec_command(test_cmd);
    let duration = start.elapsed().as_millis() as u64;

    let (tests_passed, tests_total) = parse_test_output(&output.stdout, &cmds.stack);
    let exit_ok = output.exit_code == 0;

    let score = if tests_total > 0 {
        tests_passed as f64 / tests_total as f64
    } else if exit_ok {
        1.0
    } else {
        0.0
    };

    let passed = exit_ok && score >= cmds.min_pass_rate;
    let verdict = if !passed {
        "FAIL"
    } else if score >= 1.0 {
        "PASS"
    } else {
        "WARN"
    };

    DimResult {
        dimension: "correctness".into(),
        score,
        passed,
        verdict: verdict.into(),
        details: json!({
            "command": test_cmd,
            "exit_code": output.exit_code,
            "tests_passed": tests_passed,
            "tests_total": tests_total,
            "min_pass_rate": cmds.min_pass_rate,
            "stdout_tail": truncate(&output.stdout, 500),
            "stderr_tail": truncate(&output.stderr, 500),
        }),
        duration_ms: duration,
    }
}

// ── Performance ─────────────────────────────────────────────────────

pub fn run_performance(cmds: &ResolvedCommands) -> DimResult {
    let start = Instant::now();

    let bench_cmd = match &cmds.bench_command {
        Some(c) => c,
        None => {
            return DimResult {
                dimension: "performance".into(),
                score: 0.0,
                passed: false,
                verdict: "SKIPPED".into(),
                details: json!({"reason": "no bench command configured or detected"}),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let output = exec_command(bench_cmd);
    let duration = start.elapsed().as_millis() as u64;
    let exit_ok = output.exit_code == 0;
    let parsed = parse_last_json_object(&output.stdout);
    let metric = ["latency_ms", "throughput", "duration_ms"]
        .into_iter()
        .find(|name| parsed.as_ref().and_then(|v| v.get(*name)).is_some())
        .unwrap_or("duration_ms");
    let value = if let Some(raw) = parsed.as_ref().and_then(|v| v.get(metric)) {
        raw.as_f64().filter(|v| v.is_finite() && *v >= 0.0)
    } else {
        Some(duration as f64)
    };
    let passed = exit_ok && value.is_some();

    DimResult {
        dimension: "performance".into(),
        score: if passed { 1.0 } else { 0.0 },
        passed,
        verdict: if passed { "PASS" } else { "FAIL" }.into(),
        details: json!({
            "command": bench_cmd,
            "exit_code": output.exit_code,
            "metric": metric,
            "metric_value": value,
            "higher_is_better": metric == "throughput",
            "max_regression_pct": cmds.max_regression_pct,
            "stdout_tail": truncate(&output.stdout, 500),
        }),
        duration_ms: duration,
    }
}

// ── Quality ─────────────────────────────────────────────────────────

pub fn run_quality(cmds: &ResolvedCommands) -> DimResult {
    let start = Instant::now();

    let lint_cmd = match &cmds.lint_command {
        Some(c) => c,
        None => {
            return DimResult {
                dimension: "quality".into(),
                score: 0.0,
                passed: false,
                verdict: "SKIPPED".into(),
                details: json!({"reason": "no lint command configured or detected"}),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let output = exec_command(lint_cmd);
    let duration = start.elapsed().as_millis() as u64;

    let exit_ok = output.exit_code == 0;

    // Lint score: 1.0 if clean, 0.0 if errors
    let lint_score = if exit_ok { 1.0 } else { 0.0 };

    // LLM-as-judge is handled by SKILL.md (LLM session). CLI marks it as deferred.
    DimResult {
        dimension: "quality".into(),
        score: lint_score,
        passed: exit_ok,
        verdict: if exit_ok { "PASS" } else { "FAIL" }.into(),
        details: json!({
            "command": lint_cmd,
            "exit_code": output.exit_code,
            "lint_score": lint_score,
            "llm_judge": "SKIPPED",
            "stdout_tail": truncate(&output.stdout, 500),
            "stderr_tail": truncate(&output.stderr, 500),
        }),
        duration_ms: duration,
    }
}

// ── Regression ──────────────────────────────────────────────────────

pub fn compute_regression(
    results: &[DimResult],
    baseline: Option<&serde_json::Value>,
    threshold: f64,
) -> DimResult {
    let start = Instant::now();

    let Some(prev) = baseline.and_then(|b| b.get("dimensions")) else {
        return DimResult {
            dimension: "regression".into(),
            score: 1.0,
            passed: true,
            verdict: "PASS".into(),
            details: json!({"note": "no previous baseline — first run"}),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    };

    let mut deltas = serde_json::Map::new();
    let mut any_regression = false;
    let mut comparison_error = false;

    for r in results {
        if r.verdict == "SKIPPED" {
            continue;
        }
        if let Some(prev_dim) = prev.get(&r.dimension) {
            if prev_dim.get("verdict").and_then(Value::as_str) == Some("SKIPPED") {
                continue;
            }
            if r.dimension == "performance" {
                let delta = performance_delta(r, prev_dim);
                any_regression |= delta["regressed"].as_bool().unwrap_or(true);
                comparison_error |= delta["comparison_error"].as_bool().unwrap_or(false);
                deltas.insert(r.dimension.clone(), delta);
                continue;
            }
            let prev_score = prev_dim
                .get("score")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);
            let delta = r.score - prev_score;
            let regressed = delta < -threshold;
            if regressed {
                any_regression = true;
            }
            deltas.insert(
                r.dimension.clone(),
                json!({
                    "baseline": prev_score,
                    "current": r.score,
                    "delta": format!("{:+.4}", delta),
                    "regressed": regressed,
                }),
            );
        }
    }

    // An existing baseline with no comparable inputs is not a passing check.
    if deltas.is_empty() {
        any_regression = true;
    }
    let score = if any_regression { 0.0 } else { 1.0 };

    DimResult {
        dimension: "regression".into(),
        score,
        passed: !any_regression,
        verdict: if any_regression { "FAIL" } else { "PASS" }.into(),
        details: json!({ "deltas": deltas, "threshold": threshold, "comparison_error": comparison_error }),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn performance_delta(current: &DimResult, previous: &Value) -> Value {
    let invalid = || {
        json!({"regressed": true, "comparison_error": true,
        "reason": "performance baseline and current measurement are not comparable"})
    };
    let Some(metric) = current.details["metric"].as_str() else {
        return invalid();
    };
    let previous_value = if previous["details"]["metric"].as_str() == Some(metric) {
        previous["details"]["metric_value"].as_f64()
    } else {
        if previous["details"].get("metric").is_some() {
            return invalid();
        }
        previous["details"]["stdout_tail"]
            .as_str()
            .and_then(parse_last_json_object)
            .and_then(|v| v.get(metric).and_then(Value::as_f64))
            .or_else(|| {
                (metric == "duration_ms")
                    .then(|| previous["duration_ms"].as_f64())
                    .flatten()
            })
    };
    let Some((before, after)) = previous_value
        .zip(current.details["metric_value"].as_f64())
        .filter(|(a, b)| a.is_finite() && b.is_finite() && *a >= 0.0 && *b >= 0.0)
    else {
        return invalid();
    };
    let higher_is_better = current.details["higher_is_better"]
        .as_bool()
        .unwrap_or(false);
    let limit = current.details["max_regression_pct"]
        .as_f64()
        .unwrap_or(10.0);
    let deterioration_pct = (before > 0.0).then(|| {
        (if higher_is_better {
            before - after
        } else {
            after - before
        }) / before
            * 100.0
    });
    let regressed = deterioration_pct.map_or(!higher_is_better && after > 0.0, |p| p > limit);
    json!({"metric": metric, "baseline": before, "current": after,
        "delta": format!("{:+.4}", after-before), "deterioration_pct": deterioration_pct,
        "max_regression_pct": limit, "higher_is_better": higher_is_better,
        "regressed": regressed, "comparison_error": false})
}

// ── Benchmark ───────────────────────────────────────────────────────

/// Run a single configured benchmark command and return its result.
pub fn run_benchmark(bench: &Benchmark) -> DimResult {
    let start = Instant::now();
    let output = exec_command(&bench.command);
    let duration = start.elapsed().as_millis() as u64;
    let exit_ok = output.exit_code == 0;

    let parsed_score = match bench.result_type.as_str() {
        "composite" => parse_composite_score(&output.stdout),
        "exit_code" => Some(if exit_ok { 1.0 } else { 0.0 }),
        _ => None,
    };
    let score = parsed_score.unwrap_or(0.0);
    let passed = exit_ok && parsed_score.is_some();
    let verdict = if passed { "PASS" } else { "FAIL" };

    DimResult {
        dimension: format!("benchmark:{}", bench.name),
        score,
        passed,
        verdict: verdict.into(),
        details: json!({
            "command": bench.command,
            "result_type": bench.result_type,
            "exit_code": output.exit_code,
            "score": score,
            "invalid_result": parsed_score.is_none(),
            "stdout_tail": truncate(&output.stdout, 500),
            "stderr_tail": truncate(&output.stderr, 200),
        }),
        duration_ms: duration,
    }
}

/// Parse a full JSON object or the last complete JSON object in logged stdout.
/// Invalid/missing scores never become fabricated perfect results.
fn parse_composite_score(stdout: &str) -> Option<f64> {
    fn score(value: &Value) -> Option<f64> {
        value
            .get("composite")
            .or_else(|| value.get("score"))
            .and_then(Value::as_f64)
            .filter(|s| s.is_finite() && (0.0..=1.0).contains(s))
    }
    parse_last_json_object(stdout).as_ref().and_then(score)
}

fn parse_last_json_object(stdout: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        return Some(value);
    }
    let mut offset = 0;
    let mut consumed_until = 0;
    let mut last = None;
    for line in stdout.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let start = offset + line.len() - trimmed.len();
        offset += line.len();
        if start < consumed_until || !trimmed.starts_with('{') {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next() {
            consumed_until = start + stream.byte_offset();
            last = Some(value);
        }
    }
    last
}

// ── Helpers ─────────────────────────────────────────────────────────

struct CmdOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn exec_command(cmd: &str) -> CmdOutput {
    // Split command into program + args for safe execution
    let parts = shell_words(cmd);
    let (program, args) = match parts.split_first() {
        Some((p, a)) => (p.as_str(), a.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
        None => {
            return CmdOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("empty command: {cmd}"),
            };
        }
    };

    let output = std::process::Command::new(program).args(&args).output();

    match output {
        Ok(o) => CmdOutput {
            exit_code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => CmdOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("failed to execute '{cmd}': {e}"),
        },
    }
}

/// Split program/argv without shell expansion. Preserve empty quoted arguments,
/// escaped quote characters and ordinary native path backslashes.
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        // A native Windows UNC/device root begins with two literal separators.
        // Keep that spelling while also accepting the escaped four-slash form.
        let native_unc_root = cfg!(windows)
            && current.is_empty()
            && !in_single
            && ch == '\\'
            && chars.peek() == Some(&'\\')
            && chars
                .clone()
                .nth(1)
                .is_some_and(|next| !next.is_whitespace() && !matches!(next, '\\' | '"' | '\''));
        if native_unc_root {
            current.push(ch);
            current.push(chars.next().expect("peeked separator"));
            started = true;
            continue;
        }
        if ch == '\\' && !in_single {
            let escaped = chars.peek().is_some_and(|next| {
                if in_double {
                    matches!(next, '"' | '\\')
                } else {
                    matches!(next, '"' | '\'' | '\\' | ' ' | '\t' | '\n')
                }
            });
            if escaped {
                current.push(chars.next().expect("peeked character"));
                started = true;
                continue;
            }
        }
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(ch);
                started = true;
            }
        }
    }
    if in_single || in_double {
        return Vec::new();
    }
    if started {
        words.push(current);
    }
    words
}

/// Parse test output for pass/fail counts.
fn parse_test_output(stdout: &str, stack: &str) -> (usize, usize) {
    match stack {
        "rust" => parse_rust_test_output(stdout),
        "node" => parse_node_test_output(stdout),
        "python" => parse_python_test_output(stdout),
        "go" => parse_go_test_output(stdout),
        _ => (0, 0),
    }
}

fn parse_rust_test_output(stdout: &str) -> (usize, usize) {
    // "test result: ok. 142 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    for line in stdout.lines().rev() {
        if line.contains("test result:") {
            let passed = extract_number(line, "passed").unwrap_or(0);
            let failed = extract_number(line, "failed").unwrap_or(0);
            return (passed, passed + failed);
        }
    }
    (0, 0)
}

fn parse_node_test_output(stdout: &str) -> (usize, usize) {
    // "Tests:  142 passed, 0 failed" (vitest/jest)
    for line in stdout.lines().rev() {
        if line.contains("passed") {
            let passed = extract_number(line, "passed").unwrap_or(0);
            let failed = extract_number(line, "failed").unwrap_or(0);
            return (passed, passed + failed);
        }
    }
    (0, 0)
}

fn parse_python_test_output(stdout: &str) -> (usize, usize) {
    // "142 passed, 0 failed" (pytest)
    for line in stdout.lines().rev() {
        if line.contains("passed") {
            let passed = extract_number(line, "passed").unwrap_or(0);
            let failed = extract_number(line, "failed")
                .or_else(|| extract_number(line, "error").or(Some(0)))
                .unwrap_or(0);
            return (passed, passed + failed);
        }
    }
    (0, 0)
}

fn parse_go_test_output(stdout: &str) -> (usize, usize) {
    // "ok  github.com/pkg  0.123s" or "FAIL  github.com/pkg  0.123s"
    let ok_count = stdout.lines().filter(|l| l.starts_with("ok\t")).count();
    let fail_count = stdout.lines().filter(|l| l.starts_with("FAIL\t")).count();
    (ok_count, ok_count + fail_count)
}

fn extract_number(haystack: &str, needle: &str) -> Option<usize> {
    // Find "needle" and extract the number just before it
    let needle_with_space = format!(" {needle}");
    if let Some(pos) = haystack.rfind(&needle_with_space) {
        let before = &haystack[..pos];
        let num_str: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return num_str.parse().ok();
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find a safe char boundary for the tail
    let end = s.len() - max;
    let end = (0..=end)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    format!("...{}", &s[end..])
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn command_words_preserve_empty_arguments_escaped_quotes_and_native_paths() {
        assert_eq!(
            shell_words(r##"tool "" "alpha \"beta\"" 'literal\path' "C:\space path\file""##),
            [
                "tool",
                "",
                "alpha \"beta\"",
                r"literal\path",
                r"C:\space path\file"
            ]
        );
        assert!(
            shell_words("tool \"unfinished").is_empty(),
            "malformed quotes must not execute"
        );
    }

    #[cfg(windows)]
    #[test]
    fn command_words_preserve_native_unc_and_extended_windows_roots() {
        assert_eq!(
            shell_words(
                r#""\\server\share\tool.exe" "\\server\share\file name" "\\?\C:\long path\file" "\\.\pipe\epic""#,
            ),
            [
                r"\\server\share\tool.exe",
                r"\\server\share\file name",
                r"\\?\C:\long path\file",
                r"\\.\pipe\epic",
            ]
        );
        assert_eq!(
            shell_words(r#"tool "\\\\server\\share" "\\""#),
            ["tool", r"\\server\share", r"\"]
        );
    }
}
