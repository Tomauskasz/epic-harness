mod config;
mod eval;
mod evolve;
mod harness_cli;
mod hooks;
mod mem;
mod orbit_cli;
mod orchestrate;
mod serve;
mod shared;
mod store;
mod team;
mod telemetry;
mod update;

use std::env;
use std::io::{self, IsTerminal, Read};

use epic_harness::codex;

const HOOK_STDIN_MAX_BYTES: usize = 1024 * 1024;

fn read_hook_input(mut reader: impl Read) -> Result<(hooks::common::HookInput, String), String> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((HOOK_STDIN_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read hook input: {error}"))?;

    if bytes.len() > HOOK_STDIN_MAX_BYTES {
        return Err(format!(
            "hook input exceeds {HOOK_STDIN_MAX_BYTES} byte limit"
        ));
    }
    if bytes.is_empty() {
        return Ok((hooks::common::HookInput::default(), String::new()));
    }

    let input = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid hook input JSON: {error}"))?;
    let raw =
        String::from_utf8(bytes).map_err(|error| format!("hook input is not UTF-8: {error}"))?;
    Ok((input, raw))
}

fn codex_guard_deny(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

fn is_hook_subcommand(subcmd: &str) -> bool {
    matches!(
        subcmd,
        "resume" | "guard" | "polish" | "observe" | "snapshot" | "reflect"
    )
}

fn validate_hook_input_for_subcommand(
    subcmd: &str,
    input: &hooks::common::HookInput,
    raw: &str,
) -> Result<(), String> {
    if subcmd != "guard" {
        return Ok(());
    }
    if raw.trim().is_empty() {
        return Err("guard hook input is empty".into());
    }
    if let Some(event) = input.hook_event_name.as_deref()
        && event != "PreToolUse"
    {
        return Err(format!("guard requires PreToolUse input, received {event}"));
    }

    let tool_name = input
        .tool_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "guard hook input is missing tool_name".to_string())?;
    let tool_input = input
        .tool_input
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "guard hook input requires an object tool_input".to_string())?;

    match tool_name.to_ascii_lowercase().as_str() {
        "bash" => {
            let command = tool_input
                .get("command")
                .and_then(serde_json::Value::as_str)
                .filter(|command| !command.trim().is_empty());
            if command.is_none() {
                return Err("Bash guard input requires a nonempty command".into());
            }
        }
        "edit" | "write" | "multiedit" => {
            let file_path = tool_input
                .get("file_path")
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.trim().is_empty());
            if file_path.is_none() {
                return Err(format!("{tool_name} guard input requires file_path"));
            }
        }
        "notebookedit" => {
            let notebook_path = tool_input
                .get("notebook_path")
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.trim().is_empty());
            if notebook_path.is_none() {
                return Err("NotebookEdit guard input requires notebook_path".into());
            }
        }
        "apply_patch" => {
            if hooks::polish::target_files(input).is_empty() {
                return Err("apply_patch guard input requires at least one target".into());
            }
        }
        _ => return Err(format!("unsupported guard tool: {tool_name}")),
    }
    Ok(())
}

fn validate_established_session_identity(subcmd: &str) -> Result<(), String> {
    if subcmd == "resume" || shared::host::session_id().is_none() {
        return Ok(());
    }
    if !matches!(
        subcmd,
        "guard" | "polish" | "observe" | "snapshot" | "reflect"
    ) {
        return Ok(());
    }
    shared::helpers::try_session_id()
        .map(|_| ())
        .map_err(|error| format!("host session identity is unavailable: {error}"))
}

fn version_line() -> String {
    format!(
        "epic-harness {} runtime-revision {} build-identity {}",
        env!("CARGO_PKG_VERSION"),
        env!("EPIC_HARNESS_RUNTIME_REVISION"),
        env!("EPIC_HARNESS_BUILD_IDENTITY")
    )
}

fn version_json() -> String {
    serde_json::json!({
        "release_version": env!("CARGO_PKG_VERSION"),
        "runtime_revision": env!("EPIC_HARNESS_RUNTIME_REVISION"),
        "build_identity": env!("EPIC_HARNESS_BUILD_IDENTITY"),
    })
    .to_string()
}

/// Parse `--flag <value>` or `--flag=<value>` → Option<u32>
fn parse_flag_u32(args: &[String], flag: &str) -> Option<u32> {
    let eq = format!("{flag}=");
    args.iter()
        .find(|a| a.starts_with(&eq))
        .and_then(|a| a[eq.len()..].parse().ok())
        .or_else(|| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
        })
}

/// Parse `--flag <value>` or `--flag=<value>` → Option<String>
fn parse_flag_str(args: &[String], flag: &str) -> Option<String> {
    let eq = format!("{flag}=");
    args.iter()
        .find(|a| a.starts_with(&eq))
        .map(|a| a[eq.len()..].to_string())
        .or_else(|| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        })
}

/// Parse repeated `--flag <value>` → Vec<String>
fn parse_flag_multi(args: &[String], flag: &str) -> Vec<String> {
    let eq = format!("{flag}=");
    let mut results = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with(&eq) {
            results.push(args[i][eq.len()..].to_string());
        } else if args[i] == flag
            && let Some(val) = args.get(i + 1)
            && !val.starts_with('-')
        {
            results.push(val.clone());
            i += 1;
        }
        i += 1;
    }
    results
}

fn run_codex_cli(args: &[String]) -> i32 {
    if args
        .first()
        .is_some_and(|argument| matches!(argument.as_str(), "help" | "--help" | "-h"))
    {
        eprintln!("USAGE:");
        eprintln!(
            "  epic-harness codex doctor [--json] [--repair] [--plugin-root <absolute-path>]"
        );
        return 0;
    }
    if args.first().map(String::as_str) != Some("doctor") {
        eprintln!("error: expected `epic-harness codex doctor`");
        return 1;
    }

    let mut json = false;
    let mut repair = false;
    let mut plugin_root = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if !json => json = true,
            "--repair" if !repair => repair = true,
            "--plugin-root" if plugin_root.is_none() => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("error: --plugin-root requires an absolute path");
                    return 1;
                };
                plugin_root = Some(std::path::PathBuf::from(value));
                index += 1;
            }
            "--help" | "-h" => {
                eprintln!("USAGE:");
                eprintln!(
                    "  epic-harness codex doctor [--json] [--repair] [--plugin-root <absolute-path>]"
                );
                return 0;
            }
            argument => {
                eprintln!("error: unsupported codex doctor argument `{argument}`");
                return 1;
            }
        }
        index += 1;
    }

    let options = codex::DiagnoseOptions { plugin_root };
    if repair {
        match codex::repair(&options) {
            Ok(report) => {
                if json {
                    match serde_json::to_string_pretty(&report) {
                        Ok(output) => println!("{output}"),
                        Err(error) => {
                            eprintln!(
                                "codex doctor could not serialize its repair report: {error}"
                            );
                            return 1;
                        }
                    }
                } else {
                    println!(
                        "Codex bundle repair: {}",
                        if report.repaired {
                            "completed"
                        } else {
                            "not required"
                        }
                    );
                    println!("{}", report.diagnosis_after.render_human());
                    println!("{}", report.atomicity_note);
                }
                if report.diagnosis_after.healthy { 0 } else { 1 }
            }
            Err(error) => {
                eprintln!("codex doctor repair failed: {error}");
                1
            }
        }
    } else {
        let report = codex::diagnose(&options);
        if json {
            match serde_json::to_string_pretty(&report) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("codex doctor could not serialize its report: {error}");
                    return 1;
                }
            }
        } else {
            println!("{}", report.render_human());
        }
        if report.healthy { 0 } else { 1 }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let subcmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    if subcmd == "--version" || subcmd == "-v" {
        eprintln!("{}", version_line());
        std::process::exit(0);
    }
    if subcmd == "version" && args.get(2).is_some_and(|argument| argument == "--json") {
        println!("{}", version_json());
        std::process::exit(0);
    }

    // telemetry: skip consent check (it sets it).
    if subcmd == "telemetry" {
        let code = telemetry::run_cli(&args[2..]);
        std::process::exit(code);
    }
    if subcmd == "update" {
        let code = update::run(&args[2..]);
        std::process::exit(code);
    }
    if subcmd == "codex" {
        std::process::exit(run_codex_cli(&args[2..]));
    }

    // All other subcommands: auto-enable telemetry on first run (opt-out model).
    // Prints a one-time notice if consent was not yet set.
    telemetry::ensure_consent_or_set_default();

    if subcmd == "mem" {
        let code = mem::run(&args[1..]);
        std::process::exit(code);
    }
    if subcmd == "team" {
        let code = team::run(&args[1..]);
        std::process::exit(code);
    }
    if subcmd == "org" {
        let code = team::run_org(&args[1..]);
        std::process::exit(code);
    }
    if subcmd == "evolve" {
        let code = evolve::cli::run(&args[1..]);
        std::process::exit(code);
    }
    if subcmd == "eval" {
        let code = eval::run(&args[2..]);
        std::process::exit(code);
    }
    if subcmd == "harness" {
        let code = harness_cli::run(&args[1..]);
        std::process::exit(code);
    }
    if subcmd == "orbit" {
        let code = orbit_cli::run(&args[2..]);
        std::process::exit(code);
    }
    if subcmd == "serve" {
        let port = parse_flag_u32(&args, "--port").map(|p| p as u16);
        std::process::exit(serve::run_serve(port));
    }
    if subcmd == "dashboard" {
        let port = parse_flag_u32(&args, "--port").map(|p| p as u16);
        std::process::exit(serve::run_dashboard(port));
    }

    // reflect --context: data collection mode (no stdin needed)
    if subcmd == "reflect" {
        let has_context = args
            .iter()
            .any(|a| a == "--context" || a.starts_with("--context="));
        if has_context {
            // --days <N> or --context=<N> or --context <N>
            let days: u32 = parse_flag_u32(&args, "--days")
                .or_else(|| {
                    args.iter()
                        .find(|a| a.starts_with("--context="))
                        .and_then(|a| a.strip_prefix("--context=").filter(|s| !s.is_empty()))
                        .and_then(|s| s.parse().ok())
                })
                .or_else(|| parse_flag_u32(&args, "--context"))
                .unwrap_or(30);

            // --since <YYYYMMDD>
            let since: Option<String> = parse_flag_str(&args, "--since");

            // --project <slug>
            let project: Option<String> = parse_flag_str(&args, "--project");

            // --all-projects
            let all_projects = args.iter().any(|a| a == "--all-projects");

            // --source <name>  (may appear multiple times)
            let sources: Vec<String> = parse_flag_multi(&args, "--source");

            std::process::exit(hooks::reflect::run_context(
                days,
                since,
                project,
                all_projects,
                sources,
            ));
        }
    }

    // Read stdin once, pass to hook subcommands (skip if TTY — no EOF would arrive)
    let (input, stdin_buf) = if is_hook_subcommand(subcmd) {
        // Read stdin once for hook subcommands (skip if TTY - no EOF would arrive).
        // Ordinary CLI subcommands must leave stdin to their own contracts.
        let parsed_input = if io::stdin().is_terminal() {
            Ok((hooks::common::HookInput::default(), String::new()))
        } else {
            read_hook_input(io::stdin().lock())
        };
        match parsed_input {
            Ok((input, raw)) => {
                if let Err(error) = validate_hook_input_for_subcommand(subcmd, &input, &raw) {
                    eprintln!("[{subcmd}] invalid hook input: {error}");
                    exit_with_cleanup(1, true);
                }
                (input, raw)
            }
            Err(error) => {
                eprintln!("[{subcmd}] {error}");
                exit_with_cleanup(1, true);
            }
        }
    } else {
        (hooks::common::HookInput::default(), String::new())
    };

    // Decide once whether human-facing output belongs on stdout (Codex reads it
    // as model context for some events) or stderr, and record the host-supplied
    // session/agent ids. Must happen before any hook runs, since `hint`/`raw`
    // and `session_id()` consult it.
    shared::host::init(&input);

    let identity_gap = validate_established_session_identity(subcmd).err();

    let mut guard_outcome = None;
    let mut exit_code = match identity_gap {
        // `guard` is the one hook whose exit code decides whether the user's
        // tool call runs at all. A gap in the harness's own session bookkeeping
        // is not a safety condition. Upgrading the plugin mid-session can leave
        // no record for the running session, but that bookkeeping gap must not
        // become a policy denial.
        //
        // Report the gap and run the safety rules anyway. `session_id()` falls
        // back to today's date, so the only cost is a session that spans
        // midnight being split across two partitions — a far smaller failure
        // than refusing to let anyone work.
        Some(error) if subcmd == "guard" => {
            eprintln!("[guard] {error}; continuing with today's date");
            let outcome = hooks::guard::evaluate(&input);
            let exit_code = outcome.exit_code;
            guard_outcome = Some(outcome);
            exit_code
        }
        Some(error) => {
            eprintln!("[{subcmd}] {error}");
            1
        }
        None => match subcmd {
            "resume" => hooks::resume::run(&input),
            "guard" => {
                let outcome = hooks::guard::evaluate(&input);
                let exit_code = outcome.exit_code;
                guard_outcome = Some(outcome);
                exit_code
            }
            "polish" => hooks::polish::run(&input),
            "observe" => hooks::observe::run(&input),
            "snapshot" => hooks::snapshot::run(&input),
            "reflect" => hooks::reflect::run(&input),
            "migrate" => {
                let dry_run = args.iter().any(|a| a == "--dry-run");
                let reset = args.iter().any(|a| a == "--reset");
                let to_global = args.iter().any(|a| a == "--to-global");
                if to_global {
                    store::migrate::run_to_global(dry_run)
                } else {
                    store::migrate::run_subcommand(dry_run, reset)
                }
            }
            "merge-project" => {
                let from = parse_flag_str(&args, "--from");
                let to = parse_flag_str(&args, "--to");
                let dry_run = args.iter().any(|a| a == "--dry-run");
                let delete_source = args.iter().any(|a| a == "--delete-source");
                match (from, to) {
                    (Some(f), Some(t)) => {
                        store::merge_project::run_merge(&f, &t, dry_run, delete_source)
                    }
                    _ => {
                        eprintln!(
                            "Usage: epic-harness merge-project --from <slug> --to <slug> [--dry-run] [--delete-source]"
                        );
                        1
                    }
                }
            }
            "mem" | "team" | "org" | "eval" | "harness" | "orbit" | "telemetry" | "serve"
            | "dashboard" | "update" => {
                unreachable!()
            }
            "path" => {
                println!("{}", hooks::common::harness_dir().display());
                0
            }
            "slug" => {
                println!("{}", shared::paths::project_slug());
                0
            }
            "version" => {
                eprintln!("{}", version_line());
                0
            }
            _ => {
                let is_unknown = !matches!(subcmd, "help" | "--help" | "-h");
                if is_unknown {
                    eprintln!("error: unknown subcommand '{subcmd}'\n");
                }
                eprintln!(
                    "epic-harness {} — Self-evolving agent harness\n",
                    env!("CARGO_PKG_VERSION")
                );
                eprintln!("USAGE:");
                eprintln!("  epic-harness <SUBCOMMAND> [OPTIONS]\n");
                eprintln!("HOOK SUBCOMMANDS (invoked automatically by agent hooks):");
                eprintln!("  resume       Restore session context on conversation start");
                eprintln!("  guard        Block/warn on dangerous shell commands");
                eprintln!("  observe      Record tool call observations for pattern analysis");
                eprintln!("  polish       Auto-format and typecheck after file edits");
                eprintln!("  snapshot     Save session state mid-conversation");
                eprintln!("  reflect      Analyze observations and evolve skills (session end)");
                eprintln!(
                    "  reflect --context [OPTIONS]  Collect harness data as JSON for /reflect skill"
                );
                eprintln!("    --days <N>           Analysis window in days (default: 30)");
                eprintln!("    --since <YYYYMMDD>   Start date (overrides --days)");
                eprintln!("    --project <slug>     Specific project slug");
                eprintln!("    --all-projects       All projects under ~/.harness/projects/");
                eprintln!(
                    "    --source <name>      Extra context source: harness|claude-session|alcove|all (repeatable)\n"
                );
                eprintln!("EVOLUTION:");
                eprintln!("  evolve       Skill synthesis handshake (host-agent)");
                eprintln!("    accept-synth --skill <name> [--file <path> | --stdin]");
                eprintln!(
                    "                       Apply a synthesized body to a pending-synth manifest"
                );
                eprintln!("USER SUBCOMMANDS:");
                eprintln!(
                    "  eval         Project quality & regression evaluation  (epic eval --init)"
                );
                eprintln!("    --init             Scaffold eval.yaml config");
                eprintln!("    --scaffold         Generate stack-appropriate benchmark files");
                eprintln!(
                    "                       Supports: rust python typescript node go java kotlin"
                );
                eprintln!("                                 ruby php csharp swift elixir cpp");
                eprintln!("    --json             Output as JSON (for CI)");
                eprintln!("    --baseline-update  Save current results as new baseline");
                eprintln!("    --dimension <dim>  Run specific dimension only");
                eprintln!("  migrate      Import legacy JSONL/JSON data into harness.db");
                eprintln!(
                    "    --to-global          Merge per-project harness.db files into global DB"
                );
                eprintln!("    --dry-run            Preview without writing");
                eprintln!("    --reset              Retry interrupted migration");
                eprintln!("  merge-project  Consolidate two project slugs into one");
                eprintln!("    --from <slug>        Source slug to merge from");
                eprintln!("    --to <slug>          Target slug to merge into");
                eprintln!("    --dry-run            Preview without writing");
                eprintln!("    --delete-source      Remove source directory after merge");
                eprintln!("  org          Browse org team libraries  (epic org help)");
                eprintln!("  team         Manage org-level agent teams  (epic team help)");
                eprintln!(
                    "  orbit complete  Reserved for SessionEnd; manual invocation is rejected"
                );
                eprintln!("  mem          Cross-agent unified memory  (harness mem help)");
                eprintln!(
                    "  harness      Harness state as a first-class object  (epic harness snapshot|diff|restore)"
                );
                eprintln!("  dashboard    Open web dashboard in browser (default port: 7700)");
                eprintln!("  serve        Start dashboard web server without opening browser");
                eprintln!("  update       Self-update to the latest release");
                eprintln!(
                    "  codex doctor Diagnose the installed Codex bundle; add --repair to repair it"
                );
                eprintln!("  telemetry    Manage telemetry consent  (on|off|status)");
                eprintln!("  path         Print the harness data directory");
                eprintln!("  slug         Print the current project slug (worktree-safe)");
                eprintln!("  version      Print version");
                eprintln!(
                    "    --json             Print release, runtime, and build identity as JSON"
                );
                eprintln!("  --version, -v  Print version\n");
                eprintln!("Run 'epic-harness mem help' for memory subcommand details.");
                if is_unknown { 1 } else { 0 }
            }
        },
    };

    // stdout output is chosen by EVENT, not by host. Claude Code and Codex
    // both send `hook_event_name` and read the same structured
    // shapes, so this arm is the live path on every supported host. The `else`
    // arm is the no-event-name fallback (direct CLI runs), not "Claude Code".
    if input.hook_event_name.is_some() {
        match subcmd {
            "guard" if exit_code == 2 => {
                if let Some(reason) = guard_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.permission_decision_reason.as_deref())
                {
                    // The reason may come from project configuration. Serialize
                    // it as data so it cannot alter the response structure.
                    println!("{}", codex_guard_deny(reason));
                } else {
                    eprintln!("[guard] policy denial did not include a reason");
                    exit_code = 1;
                }
            }
            "observe"
                if exit_code != 0 && input.hook_event_name.as_deref() == Some("SubagentStop") =>
            {
                println!("{{}}");
            }
            "guard" | "observe" | "polish" => {
                // PreToolUse pass or PostToolUse: plain text ignored, no stdout needed
            }
            "resume" => {
                // A leading `[` or `{` makes Codex parse SessionStart stdout as
                // JSON. Emit one valid object so tagged hints and Markdown can
                // never be misclassified as malformed structured output.
                println!("{}", shared::host::take_session_start_output());
            }
            "reflect" => {
                // SessionEnd (and Stop, if a host still maps it there): these
                // events expect structured JSON — plain text is invalid.
                println!(r#"{{"continue":true}}"#);
            }
            _ => {}
        }
    } else {
        // No host event name — echo stdin, which no consumer misreads.
        print!("{stdin_buf}");
    }

    let skip_shutdown = matches!(
        subcmd,
        "help" | "--help" | "-h" | "path" | "version" | "--version" | "-v"
    );
    exit_with_cleanup(exit_code, skip_shutdown);
}

/// Gracefully close connection pools before exit to flush WAL.
///
/// # Safety
/// Must only be called from a non-async context (e.g., `main()`).
/// `store::runtime::block_on` panics if called inside a tokio runtime.
fn exit_with_cleanup(code: i32, skip_shutdown: bool) -> ! {
    if !skip_shutdown {
        // SAFETY: Called only from main() which is not inside a tokio runtime.
        store::runtime::block_on(store::pool::shutdown());
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{
        HOOK_STDIN_MAX_BYTES, codex_guard_deny, parse_flag_multi, parse_flag_str, parse_flag_u32,
        read_hook_input, validate_hook_input_for_subcommand, version_json,
    };

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── parse_flag_u32 ────────────────────────────────
    #[test]
    fn parse_flag_u32_eq_form() {
        let args = s(&["reflect", "--context=7"]);
        assert_eq!(parse_flag_u32(&args, "--context"), Some(7));
    }

    #[test]
    fn parse_flag_u32_space_form() {
        let args = s(&["reflect", "--days", "14"]);
        assert_eq!(parse_flag_u32(&args, "--days"), Some(14));
    }

    #[test]
    fn parse_flag_u32_missing() {
        let args = s(&["reflect", "--context"]);
        assert_eq!(parse_flag_u32(&args, "--days"), None);
    }

    #[test]
    fn parse_flag_u32_invalid_value() {
        let args = s(&["reflect", "--days", "abc"]);
        assert_eq!(parse_flag_u32(&args, "--days"), None);
    }

    #[test]
    fn parse_flag_u32_eq_form_prefers_eq() {
        let args = s(&["reflect", "--days=5", "--days", "99"]);
        assert_eq!(parse_flag_u32(&args, "--days"), Some(5));
    }

    // ── parse_flag_str ────────────────────────────────
    #[test]
    fn parse_flag_str_eq_form() {
        let args = s(&["reflect", "--since=20260101"]);
        assert_eq!(parse_flag_str(&args, "--since"), Some("20260101".into()));
    }

    #[test]
    fn parse_flag_str_space_form() {
        let args = s(&["reflect", "--project", "my-project"]);
        assert_eq!(
            parse_flag_str(&args, "--project"),
            Some("my-project".into())
        );
    }

    #[test]
    fn parse_flag_str_missing() {
        let args = s(&["reflect", "--context"]);
        assert_eq!(parse_flag_str(&args, "--since"), None);
    }

    // ── parse_flag_multi ──────────────────────────────
    #[test]
    fn parse_flag_multi_single_space() {
        let args = s(&["reflect", "--source", "harness"]);
        assert_eq!(
            parse_flag_multi(&args, "--source"),
            vec!["harness".to_string()]
        );
    }

    #[test]
    fn parse_flag_multi_repeated_space() {
        let args = s(&["reflect", "--source", "harness", "--source", "alcove"]);
        assert_eq!(
            parse_flag_multi(&args, "--source"),
            vec!["harness".to_string(), "alcove".to_string()]
        );
    }

    #[test]
    fn parse_flag_multi_eq_form() {
        let args = s(&["reflect", "--source=claude-session"]);
        assert_eq!(
            parse_flag_multi(&args, "--source"),
            vec!["claude-session".to_string()]
        );
    }

    #[test]
    fn parse_flag_multi_empty() {
        let args = s(&["reflect", "--context"]);
        assert_eq!(parse_flag_multi(&args, "--source"), Vec::<String>::new());
    }

    #[test]
    fn parse_flag_multi_skips_next_flag_as_value() {
        let args = s(&["reflect", "--source", "--context", "harness"]);
        // "--context" starts with '-', so it should NOT be treated as a value
        assert_eq!(parse_flag_multi(&args, "--source"), Vec::<String>::new());
    }

    #[test]
    fn malformed_hook_input_is_rejected() {
        let malformed = br#"{"turn_id":42,"tool_input":{"command":"rm -rf /"}}"#;
        assert!(read_hook_input(&malformed[..]).is_err());
    }

    #[test]
    fn guard_denial_reason_is_serialized_as_data() {
        let response = codex_guard_deny("quote: \" and newline:\nnot JSON");
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"],
            "quote: \" and newline:\nnot JSON"
        );
    }

    #[test]
    fn structured_version_contains_the_complete_bundle_identity() {
        let parsed: serde_json::Value = serde_json::from_str(&version_json()).unwrap();
        assert_eq!(parsed["release_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            parsed["runtime_revision"],
            env!("EPIC_HARNESS_RUNTIME_REVISION")
        );
        assert_eq!(
            parsed["build_identity"],
            env!("EPIC_HARNESS_BUILD_IDENTITY")
        );
    }

    #[test]
    fn hook_input_larger_than_limit_is_rejected() {
        let oversized = vec![b' '; HOOK_STDIN_MAX_BYTES + 1];
        let error = read_hook_input(&oversized[..]).unwrap_err();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn empty_guard_input_fails_schema_validation() {
        let (input, raw) = read_hook_input(&b""[..]).unwrap();
        assert!(validate_hook_input_for_subcommand("guard", &input, &raw).is_err());
    }

    #[test]
    fn empty_object_guard_input_fails_schema_validation() {
        let (input, raw) = read_hook_input(&br#"{}"#[..]).unwrap();
        assert!(validate_hook_input_for_subcommand("guard", &input, &raw).is_err());
    }

    #[test]
    fn bash_guard_requires_a_nonempty_command() {
        for tool_input in [
            serde_json::json!({}),
            serde_json::json!({"command": ""}),
            serde_json::json!({"command": 42}),
        ] {
            let input = crate::hooks::common::HookInput {
                tool_name: Some("Bash".into()),
                tool_input: Some(tool_input),
                ..Default::default()
            };
            assert!(validate_hook_input_for_subcommand("guard", &input, "{}").is_err());
        }
    }

    #[test]
    fn write_guard_requires_a_usable_target() {
        let input = crate::hooks::common::HookInput {
            tool_name: Some("Edit".into()),
            tool_input: Some(serde_json::json!({"file_path": ""})),
            ..Default::default()
        };
        assert!(validate_hook_input_for_subcommand("guard", &input, "{}").is_err());
    }
}
