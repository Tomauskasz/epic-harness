//! Read-only diagnosis and journalled repair for a Codex plugin cache.
//!
//! A Codex cache is mutable host state.  This module therefore never treats it
//! as source authority: it compares the cache with the bytes embedded in the
//! running executable.  The command-line layer is intentionally kept outside
//! this module so its API is usable from hooks and focused filesystem tests.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
use super::bundle::plugin_cachebuster;
use super::bundle::{
    MAX_ARTIFACT_BYTES, OWNED_ARTIFACTS, canonical_json, find_artifact, parse_json,
    plugin_base_version, semantic_digest,
};

const REPORT_SCHEMA_VERSION: u32 = 1;
const JOURNAL_SCHEMA_VERSION: u32 = 1;
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_DIRECTORY_DEPTH: usize = 32;
const MAX_PATH_ENTRIES: usize = 512;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const JOURNAL_SUFFIX: &str = ".epic-doctor-repair.json";
static REPAIR_NONCE: AtomicU64 = AtomicU64::new(0);

/// Input for read-only diagnosis or repair.  `plugin_root`, when set, is the
/// exact Codex cache root to inspect; relative paths are intentionally refused.
#[derive(Debug, Clone, Default)]
pub struct DiagnoseOptions {
    pub plugin_root: Option<PathBuf>,
}

impl DiagnoseOptions {
    pub fn with_plugin_root(plugin_root: impl Into<PathBuf>) -> Self {
        Self {
            plugin_root: Some(plugin_root.into()),
        }
    }
}

/// A bounded command capture.  It is both the production command boundary and
/// the injection seam used by tests, so diagnosis tests never need Codex or a
/// real PATH.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandObservation {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

impl CommandObservation {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            error: None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(message.into()),
        }
    }

    fn is_success(&self) -> bool {
        self.exit_code == Some(0) && self.error.is_none()
    }
}

/// Host observations used by [`diagnose_with`] and [`repair_with`].
///
/// `live_commands = false` is the safe default for fixtures.  Populate
/// `executable_results` with `version --json` captures for each exact path.
#[derive(Debug, Clone)]
pub struct DoctorSeams {
    pub environment: BTreeMap<String, String>,
    pub home_dir: Option<PathBuf>,
    /// `None` lets a live seam execute `codex plugin list --json` lazily, only
    /// after explicit and PLUGIN_ROOT discovery sources are absent.
    pub codex_plugin_list: Option<CommandObservation>,
    pub path_entries: Vec<PathBuf>,
    pub current_executable: Option<PathBuf>,
    pub executable_results: BTreeMap<PathBuf, CommandObservation>,
    pub live_commands: bool,
}

impl Default for DoctorSeams {
    fn default() -> Self {
        Self {
            environment: BTreeMap::new(),
            home_dir: None,
            codex_plugin_list: Some(CommandObservation::failure(
                "Codex command was not injected",
            )),
            path_entries: Vec::new(),
            current_executable: None,
            executable_results: BTreeMap::new(),
            live_commands: false,
        }
    }
}

impl DoctorSeams {
    /// Capture the host state required for ordinary CLI execution.  This only
    /// reads process state and invokes read-only version/list commands.
    pub fn system() -> Self {
        let environment = std::env::vars().collect();
        let path_entries = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        let home_dir = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        Self {
            environment,
            home_dir,
            codex_plugin_list: None,
            path_entries,
            current_executable: std::env::current_exe().ok(),
            executable_results: BTreeMap::new(),
            live_commands: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleIdentity {
    pub release_version: String,
    pub runtime_revision: String,
    pub build_identity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub code: String,
    pub component: String,
    pub message: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub source: Option<String>,
    pub selected_cache_root: Option<String>,
    pub candidates: Vec<DiscoveryCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryCandidate {
    pub source: String,
    pub cache_root: String,
    pub marketplace: Option<String>,
    pub plugin: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReport {
    pub path: String,
    pub digest_mode: String,
    pub expected_digest: String,
    pub actual_digest: Option<String>,
    pub status: String,
    pub mismatch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestChainReport {
    pub event: String,
    pub matcher: String,
    pub subcommand: String,
    pub expected_command: String,
    pub expected_command_windows: String,
    pub actual_command: Option<String>,
    pub actual_command_windows: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutableIdentityReport {
    pub role: String,
    pub command: String,
    pub resolved_executable: Option<String>,
    pub all_path_matches: Vec<String>,
    pub observed: Option<BundleIdentity>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReport {
    pub hook_commands: Vec<String>,
    pub memory_command: Option<String>,
    pub executables: Vec<ExecutableIdentityReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalReport {
    pub path: Option<String>,
    pub status: String,
    pub phase: Option<String>,
    pub backup_root: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub healthy: bool,
    /// This separates executable/source authority from cache drift.  Repair
    /// refuses when false, but is allowed to fix unhealthy cache artifacts.
    pub identity_healthy: bool,
    pub expected_identity: BundleIdentity,
    pub selected_cache_root: Option<String>,
    pub discovery: DiscoveryReport,
    pub artifacts: Vec<ArtifactReport>,
    pub manifest_chains: Vec<ManifestChainReport>,
    pub commands: CommandReport,
    pub journal: JournalReport,
    pub issues: Vec<Issue>,
}

impl DoctorReport {
    pub fn render_human(&self) -> String {
        let root = self.selected_cache_root.as_deref().unwrap_or("<none>");
        let mut lines = vec![format!(
            "Codex bundle: {} ({root})",
            if self.healthy { "healthy" } else { "unhealthy" }
        )];
        lines.push(format!(
            "Executable/source identity: {}",
            if self.identity_healthy {
                "healthy"
            } else {
                "unhealthy"
            }
        ));
        for issue in &self.issues {
            lines.push(format!(
                "{} [{}]: {}",
                issue.code, issue.component, issue.message
            ));
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    pub repaired: bool,
    pub rolled_back: bool,
    pub selected_cache_root: String,
    pub backup_root: Option<String>,
    pub journal: JournalReport,
    pub diagnosis_before: DoctorReport,
    pub diagnosis_after: DoctorReport,
    pub atomicity_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub recovered: bool,
    pub action: String,
    pub journal: JournalReport,
}

#[derive(Debug, Clone)]
pub struct DoctorError(pub String);

impl std::fmt::Display for DoctorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DoctorError {}

/// Diagnose using real environment, Codex, PATH and executable command seams.
/// It never creates, modifies, locks, or removes a file.
pub fn diagnose(options: &DiagnoseOptions) -> DoctorReport {
    diagnose_with(options, &DoctorSeams::system())
}

/// Diagnose with injected host observations.  This is the preferred test seam.
pub fn diagnose_with(options: &DiagnoseOptions, seams: &DoctorSeams) -> DoctorReport {
    let expected_identity = compiled_identity();
    let mut issues = Vec::new();
    let source_valid = validate_identity(&expected_identity, "embedded executable", &mut issues);
    let discovery = discover(options, seams, &mut issues);
    let selected_root = discovery.selected_cache_root.as_deref().map(PathBuf::from);
    let journal = selected_root
        .as_deref()
        .map(|root| inspect_journal(root, &mut issues))
        .unwrap_or_else(|| JournalReport {
            path: None,
            status: "not_checked".to_owned(),
            phase: None,
            backup_root: None,
            detail: None,
        });

    let expected_chains = match expected_chains() {
        Ok(chains) => chains,
        Err(error) => {
            add_issue(
                &mut issues,
                "SOURCE_HOOK_MANIFEST_INVALID",
                ".codex-plugin/hooks.json",
                error,
                "rebuild from a checked executable-owned bundle manifest",
            );
            Vec::new()
        }
    };
    let (artifacts, manifest_chains) = match selected_root.as_deref() {
        Some(root) => verify_projection(root, &expected_chains, &mut issues),
        None => (
            unverified_artifact_reports(),
            expected_chains
                .iter()
                .map(|chain| ManifestChainReport {
                    event: chain.event.clone(),
                    matcher: chain.matcher.clone(),
                    subcommand: chain.subcommand.clone(),
                    expected_command: chain.command.clone(),
                    expected_command_windows: chain.command_windows.clone(),
                    actual_command: None,
                    actual_command_windows: None,
                    status: "not_checked".to_owned(),
                })
                .collect(),
        ),
    };
    let commands = inspect_commands(selected_root.as_deref(), seams, &mut issues);
    let current_ok = commands
        .executables
        .iter()
        .find(|entry| entry.role == "current_executable")
        .is_some_and(|entry| entry.status == "healthy");
    let identity_healthy = source_valid && current_ok;
    if !current_ok {
        add_issue(
            &mut issues,
            "EXECUTABLE_CURRENT_IDENTITY_UNHEALTHY",
            "current_executable",
            "the executing binary does not prove the embedded release, runtime revision, and build identity",
            "run the matching released epic-harness binary before repair",
        );
    }
    let healthy = issues.is_empty();
    DoctorReport {
        schema_version: REPORT_SCHEMA_VERSION,
        healthy,
        identity_healthy,
        expected_identity,
        selected_cache_root: discovery.selected_cache_root.clone(),
        discovery,
        artifacts,
        manifest_chains,
        commands,
        journal,
        issues,
    }
}

pub fn compiled_identity() -> BundleIdentity {
    BundleIdentity {
        release_version: env!("CARGO_PKG_VERSION").to_owned(),
        runtime_revision: env!("EPIC_HARNESS_RUNTIME_REVISION").to_owned(),
        build_identity: env!("EPIC_HARNESS_BUILD_IDENTITY").to_owned(),
    }
}

#[derive(Debug, Clone)]
struct ExpectedChain {
    event: String,
    matcher: String,
    subcommand: String,
    command: String,
    command_windows: String,
}

fn expected_chains() -> Result<Vec<ExpectedChain>, String> {
    let artifact = find_artifact(".codex-plugin/hooks.json")
        .ok_or_else(|| "embedded Codex hooks artifact is unavailable".to_owned())?;
    let value = parse_json(&artifact.expected_bytes(None)?, artifact.path)?;
    let hooks = value
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| "embedded hooks manifest has no hooks object".to_owned())?;
    let mut chains = Vec::new();
    for (event, entries) in hooks {
        let entries = entries
            .as_array()
            .ok_or_else(|| format!("embedded hooks event {event} is not an array"))?;
        for entry in entries {
            let matcher = entry
                .get("matcher")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("embedded hooks event {event} lacks a matcher"))?;
            let handlers = entry
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("embedded hooks event {event} lacks handlers"))?;
            for handler in handlers {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    return Err(format!(
                        "embedded hooks event {event} has a non-command handler"
                    ));
                }
                let command = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("embedded hooks event {event} lacks command"))?;
                let command_windows = handler
                    .get("commandWindows")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("embedded hooks event {event} lacks commandWindows"))?;
                let (_, invocation) = command.rsplit_once(" hook ").ok_or_else(|| {
                    format!("embedded hook command for {event} has no hook invocation")
                })?;
                let parts = invocation.split_whitespace().collect::<Vec<_>>();
                if parts.len() != 2 || parts[0] != event {
                    return Err(format!(
                        "embedded hook command for {event} has invalid invocation"
                    ));
                }
                chains.push(ExpectedChain {
                    event: event.clone(),
                    matcher: matcher.to_owned(),
                    subcommand: parts[1].to_owned(),
                    command: command.to_owned(),
                    command_windows: command_windows.to_owned(),
                });
            }
        }
    }
    chains.sort_by(|left, right| {
        (&left.event, &left.matcher, &left.subcommand).cmp(&(
            &right.event,
            &right.matcher,
            &right.subcommand,
        ))
    });
    if chains.len() != 9 {
        return Err(format!(
            "embedded hooks manifest has {} command chains, expected 9",
            chains.len()
        ));
    }
    let unique = chains
        .iter()
        .map(|chain| (&chain.event, &chain.matcher, &chain.subcommand))
        .collect::<BTreeSet<_>>();
    if unique.len() != chains.len() {
        return Err("embedded hooks manifest has duplicate command chains".to_owned());
    }
    Ok(chains)
}

fn add_issue(
    issues: &mut Vec<Issue>,
    code: &str,
    component: impl Into<String>,
    message: impl Into<String>,
    action: impl Into<String>,
) {
    issues.push(Issue {
        code: code.to_owned(),
        component: component.into(),
        message: message.into(),
        action: action.into(),
    });
}

fn validate_identity(identity: &BundleIdentity, component: &str, issues: &mut Vec<Issue>) -> bool {
    let valid_release = valid_semver(&identity.release_version);
    let valid_revision = valid_revision(&identity.runtime_revision);
    let valid_build = valid_build_identity(&identity.build_identity);
    if valid_release && valid_revision && valid_build {
        return true;
    }
    add_issue(
        issues,
        "SOURCE_IDENTITY_INVALID",
        component,
        "release version, runtime revision, or build identity has an invalid format",
        "rebuild from a checked bundle manifest; do not repair from an unverified executable",
    );
    false
}

fn valid_semver(value: &str) -> bool {
    value.split('.').count() == 3
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_revision(value: &str) -> bool {
    !value.is_empty() && value != "0" && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_build_identity(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn discover(
    options: &DiagnoseOptions,
    seams: &DoctorSeams,
    issues: &mut Vec<Issue>,
) -> DiscoveryReport {
    if let Some(root) = options.plugin_root.as_deref() {
        return explicit_candidate("explicit", root, issues);
    }
    if let Some(root) = seams.environment.get("PLUGIN_ROOT") {
        return explicit_candidate("PLUGIN_ROOT", Path::new(root), issues);
    }

    let plugin_list = seams.codex_plugin_list.clone().unwrap_or_else(|| {
        if seams.live_commands {
            run_bounded("codex", &["plugin", "list", "--json"])
        } else {
            CommandObservation::failure("Codex command was not injected")
        }
    });
    if !plugin_list.is_success() {
        add_issue(
            issues,
            "DISCOVERY_CODEX_LIST_FAILED",
            "codex plugin list --json",
            command_failure_detail(&plugin_list),
            "set --plugin-root to the exact absolute Codex cache directory",
        );
        return DiscoveryReport {
            source: None,
            selected_cache_root: None,
            candidates: Vec::new(),
        };
    }
    let parsed = match serde_json::from_str::<Value>(&plugin_list.stdout) {
        Ok(value) => value,
        Err(error) => {
            add_issue(
                issues,
                "DISCOVERY_CODEX_LIST_INVALID",
                "codex plugin list --json",
                format!("command output is not valid JSON: {error}"),
                "set --plugin-root to the exact absolute Codex cache directory",
            );
            return DiscoveryReport {
                source: None,
                selected_cache_root: None,
                candidates: Vec::new(),
            };
        }
    };
    let home = match seams.home_dir.as_deref() {
        Some(home) if home.is_absolute() => home,
        _ => {
            add_issue(
                issues,
                "DISCOVERY_HOME_UNAVAILABLE",
                "Codex cache root",
                "Codex plugin output cannot be mapped to the official cache without an absolute home directory",
                "set --plugin-root to the exact absolute Codex cache directory",
            );
            return DiscoveryReport {
                source: None,
                selected_cache_root: None,
                candidates: Vec::new(),
            };
        }
    };
    let candidates = plugin_list_candidates(&parsed, home);
    if candidates.is_empty() {
        add_issue(
            issues,
            "DISCOVERY_EPIC_NOT_FOUND",
            "codex plugin list --json",
            "no unambiguous epic@personal plugin named epic was found",
            "install epic@personal or set --plugin-root to the exact absolute cache directory",
        );
        return DiscoveryReport {
            source: None,
            selected_cache_root: None,
            candidates,
        };
    }
    let roots = candidates
        .iter()
        .map(|candidate| candidate.cache_root.clone())
        .collect::<BTreeSet<_>>();
    if roots.len() != 1 {
        add_issue(
            issues,
            "DISCOVERY_AMBIGUOUS",
            "codex plugin list --json",
            "more than one epic@personal cache root was reported",
            "pass --plugin-root with one exact absolute cache directory",
        );
        return DiscoveryReport {
            source: None,
            selected_cache_root: None,
            candidates,
        };
    }
    let root = roots.into_iter().next().expect("nonempty roots");
    DiscoveryReport {
        source: Some("codex_plugin_list".to_owned()),
        selected_cache_root: Some(root),
        candidates,
    }
}

fn explicit_candidate(source: &str, root: &Path, issues: &mut Vec<Issue>) -> DiscoveryReport {
    let root_display = path_display(root);
    if !root.is_absolute() {
        add_issue(
            issues,
            "DISCOVERY_ROOT_NOT_ABSOLUTE",
            source,
            format!("{root_display} is not an absolute plugin root"),
            "pass an absolute --plugin-root path",
        );
        return DiscoveryReport {
            source: Some(source.to_owned()),
            selected_cache_root: None,
            candidates: Vec::new(),
        };
    }
    DiscoveryReport {
        source: Some(source.to_owned()),
        selected_cache_root: Some(root_display.clone()),
        candidates: vec![DiscoveryCandidate {
            source: source.to_owned(),
            cache_root: root_display,
            marketplace: None,
            plugin: Some("epic".to_owned()),
            version: None,
        }],
    }
}

fn plugin_list_candidates(value: &Value, home: &Path) -> Vec<DiscoveryCandidate> {
    let mut entries = Vec::new();
    collect_plugin_objects(value, &mut entries);
    let mut candidates = Vec::new();
    for object in entries {
        let name = string_field(object, &["name", "plugin_name"]);
        let marketplace = string_field(
            object,
            &["marketplace", "marketplace_name", "marketplaceName"],
        );
        let id = string_field(object, &["id", "plugin_id", "pluginId"]);
        let version = string_field(object, &["version"]);
        let name_is_epic = name == Some("epic");
        let marketplace_is_personal =
            marketplace == Some("personal") || id == Some("epic@personal");
        let active = object.get("installed").and_then(Value::as_bool) == Some(true)
            && object.get("enabled").and_then(Value::as_bool) == Some(true);
        let Some(version) = version else { continue };
        if !name_is_epic || !marketplace_is_personal || !active || !valid_cache_version(version) {
            continue;
        }
        let root = home
            .join(".codex")
            .join("plugins")
            .join("cache")
            .join("personal")
            .join("epic")
            .join(version);
        candidates.push(DiscoveryCandidate {
            source: "codex_plugin_list".to_owned(),
            cache_root: path_display(&root),
            marketplace: Some("personal".to_owned()),
            plugin: Some("epic".to_owned()),
            version: Some(version.to_owned()),
        });
    }
    candidates.sort_by(|left, right| left.cache_root.cmp(&right.cache_root));
    candidates.dedup_by(|left, right| left.cache_root == right.cache_root);
    candidates
}

fn collect_plugin_objects<'a>(
    value: &'a Value,
    objects: &mut Vec<&'a serde_json::Map<String, Value>>,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_plugin_objects(value, objects);
            }
        }
        Value::Object(object) => {
            if object.contains_key("name") || object.contains_key("plugin_name") {
                objects.push(object);
            }
            for value in object.values() {
                collect_plugin_objects(value, objects);
            }
        }
        _ => {}
    }
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn valid_cache_version(value: &str) -> bool {
    plugin_base_version(value, "codex plugin list version").is_ok()
}

fn verify_projection(
    root: &Path,
    expected_chains: &[ExpectedChain],
    issues: &mut Vec<Issue>,
) -> (Vec<ArtifactReport>, Vec<ManifestChainReport>) {
    if let Err(error) = ensure_safe_root(root) {
        add_issue(
            issues,
            "CACHE_ROOT_UNSAFE",
            path_display(root),
            error,
            "select a real, non-symlink Codex cache root",
        );
        return (
            unverified_artifact_reports(),
            unverified_chain_reports(expected_chains),
        );
    }

    let mut artifact_bytes = BTreeMap::new();
    let mut reports = Vec::with_capacity(OWNED_ARTIFACTS.len());
    for artifact in OWNED_ARTIFACTS {
        let expected_digest = artifact
            .expected_digest()
            .unwrap_or_else(|error| format!("invalid:{error}"));
        match read_owned_file(root, artifact.path) {
            Ok(bytes) => match semantic_digest(&bytes, artifact.mode, artifact.path) {
                Ok(actual_digest) if actual_digest == expected_digest => {
                    artifact_bytes.insert(artifact.path, bytes);
                    reports.push(ArtifactReport {
                        path: artifact.path.to_owned(),
                        digest_mode: artifact.mode.name().to_owned(),
                        expected_digest,
                        actual_digest: Some(actual_digest),
                        status: "healthy".to_owned(),
                        mismatch: None,
                    });
                }
                Ok(actual_digest) => {
                    add_issue(
                        issues,
                        "ARTIFACT_MISMATCH",
                        artifact.path,
                        "semantic digest differs from the executable-owned projection",
                        "run codex doctor --repair after verifying the executable identity",
                    );
                    reports.push(ArtifactReport {
                        path: artifact.path.to_owned(),
                        digest_mode: artifact.mode.name().to_owned(),
                        expected_digest,
                        actual_digest: Some(actual_digest),
                        status: "mismatch".to_owned(),
                        mismatch: Some("semantic digest differs".to_owned()),
                    });
                }
                Err(error) => {
                    add_issue(
                        issues,
                        "ARTIFACT_INVALID",
                        artifact.path,
                        error.clone(),
                        "run codex doctor --repair after verifying the executable identity",
                    );
                    reports.push(ArtifactReport {
                        path: artifact.path.to_owned(),
                        digest_mode: artifact.mode.name().to_owned(),
                        expected_digest,
                        actual_digest: None,
                        status: "invalid".to_owned(),
                        mismatch: Some(error),
                    });
                }
            },
            Err(error) => {
                let code = if error.kind == OwnedReadErrorKind::Missing {
                    "ARTIFACT_MISSING"
                } else {
                    "ARTIFACT_PATH_UNSAFE"
                };
                add_issue(
                    issues,
                    code,
                    artifact.path,
                    error.message.clone(),
                    "run codex doctor --repair after verifying the executable identity",
                );
                reports.push(ArtifactReport {
                    path: artifact.path.to_owned(),
                    digest_mode: artifact.mode.name().to_owned(),
                    expected_digest,
                    actual_digest: None,
                    status: if error.kind == OwnedReadErrorKind::Missing {
                        "missing".to_owned()
                    } else {
                        "unsafe".to_owned()
                    },
                    mismatch: Some(error.message),
                });
            }
        }
    }
    validate_installed_manifest(
        artifact_bytes.get("registry/scripts/bundle-manifest.json"),
        issues,
    );
    let chains = verify_manifest_chains(
        artifact_bytes
            .get(".codex-plugin/hooks.json")
            .map(Vec::as_slice),
        expected_chains,
        issues,
    );
    (reports, chains)
}

fn unverified_artifact_reports() -> Vec<ArtifactReport> {
    OWNED_ARTIFACTS
        .iter()
        .map(|artifact| ArtifactReport {
            path: artifact.path.to_owned(),
            digest_mode: artifact.mode.name().to_owned(),
            expected_digest: artifact
                .expected_digest()
                .unwrap_or_else(|error| format!("invalid:{error}")),
            actual_digest: None,
            status: "not_checked".to_owned(),
            mismatch: Some("no safe selected cache root".to_owned()),
        })
        .collect()
}

fn unverified_chain_reports(expected_chains: &[ExpectedChain]) -> Vec<ManifestChainReport> {
    expected_chains
        .iter()
        .map(|chain| ManifestChainReport {
            event: chain.event.clone(),
            matcher: chain.matcher.clone(),
            subcommand: chain.subcommand.clone(),
            expected_command: chain.command.clone(),
            expected_command_windows: chain.command_windows.clone(),
            actual_command: None,
            actual_command_windows: None,
            status: "not_checked".to_owned(),
        })
        .collect()
}

fn validate_installed_manifest(bytes: Option<&Vec<u8>>, issues: &mut Vec<Issue>) {
    let Some(bytes) = bytes else {
        return;
    };
    let value = match parse_json(bytes, "registry/scripts/bundle-manifest.json") {
        Ok(value) => value,
        Err(error) => {
            add_issue(
                issues,
                "BUNDLE_MANIFEST_INVALID",
                "registry/scripts/bundle-manifest.json",
                error,
                "repair the Codex bundle from a verified executable",
            );
            return;
        }
    };
    let Some(object) = value.as_object() else {
        add_issue(
            issues,
            "BUNDLE_MANIFEST_INVALID",
            "registry/scripts/bundle-manifest.json",
            "bundle manifest must be a JSON object",
            "repair the Codex bundle from a verified executable",
        );
        return;
    };
    let expected = compiled_identity();
    let schema = object.get("schema_version").and_then(Value::as_u64);
    let release = object.get("release_version").and_then(Value::as_str);
    let revision = object.get("runtime_revision").and_then(Value::as_str);
    let build = object.get("build_identity").and_then(Value::as_str);
    if schema != Some(1)
        || release != Some(expected.release_version.as_str())
        || revision != Some(expected.runtime_revision.as_str())
        || build != Some(expected.build_identity.as_str())
    {
        add_issue(
            issues,
            "BUNDLE_MANIFEST_IDENTITY_MISMATCH",
            "registry/scripts/bundle-manifest.json",
            "schema, release, runtime revision, or build identity differs from the executable",
            "repair the Codex bundle from this verified executable",
        );
    }
    let Some(entries) = object.get("artifacts").and_then(Value::as_array) else {
        add_issue(
            issues,
            "BUNDLE_MANIFEST_ARTIFACTS_INVALID",
            "registry/scripts/bundle-manifest.json",
            "bundle manifest has no artifacts array",
            "repair the Codex bundle from this verified executable",
        );
        return;
    };
    for artifact in OWNED_ARTIFACTS {
        if artifact.path == "registry/scripts/bundle-manifest.json" {
            continue;
        }
        let expected_digest = artifact.expected_digest().unwrap_or_default();
        let matching = entries
            .iter()
            .find(|entry| entry.get("path").and_then(Value::as_str) == Some(artifact.path));
        let valid = matching.is_some_and(|entry| {
            entry.get("digest_mode").and_then(Value::as_str) == Some(artifact.mode.name())
                && entry.get("sha256").and_then(Value::as_str) == Some(expected_digest.as_str())
        });
        if !valid {
            add_issue(
                issues,
                "BUNDLE_MANIFEST_PROJECTION_MISMATCH",
                format!("registry/scripts/bundle-manifest.json: {}", artifact.path),
                "manifest artifact projection differs from the executable-owned artifact",
                "repair the Codex bundle from this verified executable",
            );
        }
    }
}

fn verify_manifest_chains(
    bytes: Option<&[u8]>,
    expected_chains: &[ExpectedChain],
    issues: &mut Vec<Issue>,
) -> Vec<ManifestChainReport> {
    let value = match bytes.map(|bytes| parse_json(bytes, ".codex-plugin/hooks.json")) {
        Some(Ok(value)) => Some(value),
        Some(Err(error)) => {
            add_issue(
                issues,
                "HOOK_MANIFEST_INVALID",
                ".codex-plugin/hooks.json",
                error,
                "repair the Codex bundle from this verified executable",
            );
            None
        }
        None => None,
    };
    expected_chains
        .iter()
        .map(|expected| {
            let actual = value.as_ref().and_then(|value| find_chain(value, expected));
            let (actual_command, actual_windows, status) = match actual {
                Some((command, windows))
                    if command == expected.command && windows == expected.command_windows =>
                {
                    (Some(command), Some(windows), "healthy".to_owned())
                }
                Some((command, windows)) => {
                    add_issue(
                        issues,
                        "HOOK_CHAIN_MISMATCH",
                        format!(
                            "{}:{}:{}",
                            expected.event, expected.matcher, expected.subcommand
                        ),
                        "hook command chain differs from the executable-owned manifest",
                        "repair the Codex bundle from this verified executable",
                    );
                    (Some(command), Some(windows), "mismatch".to_owned())
                }
                None => {
                    add_issue(
                        issues,
                        "HOOK_CHAIN_MISSING",
                        format!(
                            "{}:{}:{}",
                            expected.event, expected.matcher, expected.subcommand
                        ),
                        "required hook matcher chain is absent or malformed",
                        "repair the Codex bundle from this verified executable",
                    );
                    (None, None, "missing".to_owned())
                }
            };
            ManifestChainReport {
                event: expected.event.clone(),
                matcher: expected.matcher.clone(),
                subcommand: expected.subcommand.clone(),
                expected_command: expected.command.clone(),
                expected_command_windows: expected.command_windows.clone(),
                actual_command,
                actual_command_windows: actual_windows,
                status,
            }
        })
        .collect()
}

fn find_chain(value: &Value, expected: &ExpectedChain) -> Option<(String, String)> {
    let hooks = value.get("hooks")?.as_object()?;
    let entries = hooks.get(&expected.event)?.as_array()?;
    for entry in entries {
        if entry.get("matcher").and_then(Value::as_str) != Some(expected.matcher.as_str()) {
            continue;
        }
        for hook in entry.get("hooks")?.as_array()? {
            let command = hook.get("command").and_then(Value::as_str)?;
            let windows = hook.get("commandWindows").and_then(Value::as_str)?;
            let suffix = format!(" hook {} {}", expected.event, expected.subcommand);
            if command.ends_with(&suffix) {
                return Some((command.to_owned(), windows.to_owned()));
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnedReadErrorKind {
    Missing,
    Unsafe,
}

#[derive(Debug, Clone)]
struct OwnedReadError {
    kind: OwnedReadErrorKind,
    message: String,
}

fn read_owned_file(root: &Path, relative: &str) -> Result<Vec<u8>, OwnedReadError> {
    let path = owned_path(root, relative).map_err(unsafe_owned_error)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            OwnedReadError {
                kind: OwnedReadErrorKind::Missing,
                message: format!("{} is missing", path_display(&path)),
            }
        } else {
            unsafe_owned_error(format!("cannot inspect {}: {error}", path_display(&path)))
        }
    })?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err(unsafe_owned_error(format!(
            "{} is not a regular non-reparse file",
            path_display(&path)
        )));
    }
    if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
        return Err(unsafe_owned_error(format!(
            "{} exceeds {MAX_ARTIFACT_BYTES} byte limit",
            path_display(&path)
        )));
    }
    fs::read(&path).map_err(|error| {
        unsafe_owned_error(format!("cannot read {}: {error}", path_display(&path)))
    })
}

fn unsafe_owned_error(message: impl Into<String>) -> OwnedReadError {
    OwnedReadError {
        kind: OwnedReadErrorKind::Unsafe,
        message: message.into(),
    }
}

fn ensure_safe_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err("cache root is not absolute".to_owned());
    }
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("cannot inspect cache root {}: {error}", path_display(root)))?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err("cache root is not a real non-reparse directory".to_owned());
    }
    Ok(())
}

fn owned_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "owned path {relative:?} is not a safe relative path"
        ));
    }
    let components = relative_path.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!()
        };
        current.push(component);
        // Each pre-existing parent must remain a directory and must never
        // redirect the trusted root through a link/reparse point.
        if index + 1 < components.len() && current.exists() {
            let metadata = fs::symlink_metadata(&current)
                .map_err(|error| format!("cannot inspect {}: {error}", path_display(&current)))?;
            if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
                return Err(format!(
                    "{} is not a safe directory",
                    path_display(&current)
                ));
            }
        }
    }
    if !current.starts_with(root) {
        return Err(format!("owned path {relative:?} escapes the cache root"));
    }
    Ok(current)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn inspect_commands(
    root: Option<&Path>,
    seams: &DoctorSeams,
    issues: &mut Vec<Issue>,
) -> CommandReport {
    let hook_commands = expected_chains()
        .unwrap_or_default()
        .into_iter()
        .map(|chain| chain.command)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let expected_memory = expected_memory_command().ok();
    let memory_command = root.and_then(actual_memory_command);

    let mut executables = Vec::new();
    executables.push(inspect_current_executable(seams, issues));
    executables.push(inspect_path_program(
        "hook_runner",
        "node",
        seams,
        false,
        issues,
    ));
    executables.push(inspect_path_program(
        "hook_runtime",
        "epic-harness",
        seams,
        true,
        issues,
    ));
    let memory_program = memory_command
        .clone()
        .unwrap_or_else(|| "<unreadable>".to_owned());
    let memory_is_expected = expected_memory
        .as_deref()
        .zip(memory_command.as_deref())
        .is_some_and(|(expected, actual)| expected == actual);
    if let (Some(expected), Some(actual)) = (&expected_memory, &memory_command)
        && expected != actual
    {
        add_issue(
            issues,
            "MEMORY_COMMAND_MISMATCH",
            "mcp_config.json",
            format!("harness-mem command is {actual:?}, expected {expected:?}"),
            "repair the Codex bundle from this verified executable",
        );
    }
    if memory_command.is_none() {
        add_issue(
            issues,
            "MEMORY_COMMAND_UNREADABLE",
            "mcp_config.json",
            "harness-mem command cannot be read from the selected cache",
            "repair the Codex bundle from this verified executable",
        );
    }
    executables.push(inspect_path_program(
        "memory_runtime",
        &memory_program,
        seams,
        memory_is_expected && memory_program == "epic-harness",
        issues,
    ));

    let current = executables
        .iter()
        .find(|entry| entry.role == "current_executable")
        .and_then(|entry| entry.observed.as_ref());
    let hook = executables
        .iter()
        .find(|entry| entry.role == "hook_runtime")
        .and_then(|entry| entry.observed.as_ref());
    let memory = executables
        .iter()
        .find(|entry| entry.role == "memory_runtime")
        .and_then(|entry| entry.observed.as_ref());
    for (role, observed) in [("hook_runtime", hook), ("memory_runtime", memory)] {
        if let (Some(current), Some(observed)) = (current, observed)
            && current != observed
        {
            add_issue(
                issues,
                "EXECUTABLE_AUTHORITY_SPLIT",
                role,
                "the executable selected by the installed bundle has a different identity than the current executable",
                "install or select matching epic-harness executables before repair",
            );
        }
    }
    CommandReport {
        hook_commands,
        memory_command,
        executables,
    }
}

fn expected_memory_command() -> Result<String, String> {
    let artifact = find_artifact("mcp_config.json")
        .ok_or_else(|| "embedded mcp_config.json is unavailable".to_owned())?;
    memory_command_from_bytes(&artifact.expected_bytes(None)?)
}

fn actual_memory_command(root: &Path) -> Option<String> {
    let bytes = read_owned_file(root, "mcp_config.json").ok()?;
    memory_command_from_bytes(&bytes).ok()
}

fn memory_command_from_bytes(bytes: &[u8]) -> Result<String, String> {
    let value = parse_json(bytes, "mcp_config.json")?;
    value
        .get("mcpServers")
        .and_then(|servers| servers.get("harness-mem"))
        .and_then(|server| server.get("command"))
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "mcp_config.json has no harness-mem command".to_owned())
}

fn inspect_current_executable(
    seams: &DoctorSeams,
    issues: &mut Vec<Issue>,
) -> ExecutableIdentityReport {
    let Some(path) = seams.current_executable.as_deref() else {
        add_issue(
            issues,
            "EXECUTABLE_CURRENT_UNAVAILABLE",
            "current_executable",
            "the running executable path is unavailable",
            "run codex doctor from a released epic-harness executable",
        );
        return ExecutableIdentityReport {
            role: "current_executable".to_owned(),
            command: "<current executable>".to_owned(),
            resolved_executable: None,
            all_path_matches: Vec::new(),
            observed: None,
            status: "unavailable".to_owned(),
        };
    };
    inspect_executable_path(
        "current_executable",
        "<current executable>",
        path,
        Vec::new(),
        seams,
        true,
        issues,
    )
}

fn inspect_path_program(
    role: &str,
    program: &str,
    seams: &DoctorSeams,
    require_identity: bool,
    issues: &mut Vec<Issue>,
) -> ExecutableIdentityReport {
    if program.starts_with('<') {
        return ExecutableIdentityReport {
            role: role.to_owned(),
            command: program.to_owned(),
            resolved_executable: None,
            all_path_matches: Vec::new(),
            observed: None,
            status: "unavailable".to_owned(),
        };
    }
    let matches = all_path_matches(program, seams, issues);
    // Keep the PATH entry in `all_path_matches`, but probe the canonical
    // regular-file target.  Homebrew and similar package managers commonly
    // install command entries as symlinks.
    let resolved = matches
        .first()
        .and_then(|candidate| canonical_executable_target(candidate).ok());
    let rendered_matches = matches.iter().map(|path| path_display(path)).collect();
    match resolved {
        Some(path) if require_identity => {
            inspect_executable_path(role, program, &path, rendered_matches, seams, true, issues)
        }
        Some(path) => ExecutableIdentityReport {
            role: role.to_owned(),
            command: program.to_owned(),
            resolved_executable: Some(path_display(&path)),
            all_path_matches: rendered_matches,
            observed: None,
            status: "resolved".to_owned(),
        },
        None => {
            if require_identity {
                add_issue(
                    issues,
                    "EXECUTABLE_NOT_FOUND",
                    role,
                    format!("{program} is not present on PATH"),
                    "install the matching epic-harness executable or correct PATH",
                );
            } else {
                add_issue(
                    issues,
                    "HOOK_RUNNER_NOT_FOUND",
                    role,
                    format!("{program} is not present on PATH"),
                    "install Node.js or correct PATH",
                );
            }
            ExecutableIdentityReport {
                role: role.to_owned(),
                command: program.to_owned(),
                resolved_executable: None,
                all_path_matches: rendered_matches,
                observed: None,
                status: "missing".to_owned(),
            }
        }
    }
}

fn inspect_executable_path(
    role: &str,
    command: &str,
    path: &Path,
    all_path_matches: Vec<String>,
    seams: &DoctorSeams,
    require_identity: bool,
    issues: &mut Vec<Issue>,
) -> ExecutableIdentityReport {
    let observation = executable_observation(path, seams);
    let observed = observation.as_ref().and_then(parse_executable_identity);
    let status = match observed.as_ref() {
        Some(identity) if !require_identity || identity == &compiled_identity() => "healthy",
        Some(_) => "mismatch",
        None => "unreadable",
    };
    if require_identity && status != "healthy" {
        let detail = observation
            .as_ref()
            .map(command_failure_detail)
            .unwrap_or_else(|| "no identity observation is available".to_owned());
        add_issue(
            issues,
            if status == "mismatch" {
                "EXECUTABLE_IDENTITY_MISMATCH"
            } else {
                "EXECUTABLE_IDENTITY_UNREADABLE"
            },
            role,
            detail,
            "run the matching released epic-harness binary before repair",
        );
    }
    ExecutableIdentityReport {
        role: role.to_owned(),
        command: command.to_owned(),
        resolved_executable: Some(path_display(path)),
        all_path_matches,
        observed,
        status: status.to_owned(),
    }
}

fn executable_observation(path: &Path, seams: &DoctorSeams) -> Option<CommandObservation> {
    seams.executable_results.get(path).cloned().or_else(|| {
        seams
            .live_commands
            .then(|| run_bounded_path(path, &["version", "--json"]))
    })
}

fn parse_executable_identity(observation: &CommandObservation) -> Option<BundleIdentity> {
    if !observation.is_success() {
        return None;
    }
    let value = serde_json::from_str::<Value>(&observation.stdout).ok()?;
    let release_version = value
        .get("release_version")
        .or_else(|| value.get("version"))
        .and_then(Value::as_str)?;
    let runtime_revision = value.get("runtime_revision").and_then(Value::as_str)?;
    let build_identity = value.get("build_identity").and_then(Value::as_str)?;
    let identity = BundleIdentity {
        release_version: release_version.to_owned(),
        runtime_revision: runtime_revision.to_owned(),
        build_identity: build_identity.to_owned(),
    };
    (valid_semver(&identity.release_version)
        && valid_revision(&identity.runtime_revision)
        && valid_build_identity(&identity.build_identity))
    .then_some(identity)
}

fn all_path_matches(program: &str, seams: &DoctorSeams, issues: &mut Vec<Issue>) -> Vec<PathBuf> {
    if seams.path_entries.len() > MAX_PATH_ENTRIES {
        add_issue(
            issues,
            "PATH_SCAN_LIMIT",
            program,
            format!("PATH has more than {MAX_PATH_ENTRIES} entries"),
            "use a bounded PATH and run diagnosis again",
        );
        return Vec::new();
    }
    let names = program_names(program);
    let mut matches = Vec::new();
    for entry in &seams.path_entries {
        if !entry.is_absolute() {
            continue;
        }
        for name in &names {
            let candidate = entry.join(name);
            if canonical_executable_target(&candidate).is_ok() {
                matches.push(candidate);
            }
        }
    }
    matches
}

fn canonical_executable_target(candidate: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(candidate).map_err(|error| {
        format!(
            "cannot inspect executable {}: {error}",
            path_display(candidate)
        )
    })?;
    if is_reparse_point(&metadata) && !metadata.file_type().is_symlink() {
        return Err(format!("{} is a reparse point", path_display(candidate)));
    }
    let target = if metadata.file_type().is_symlink() {
        fs::canonicalize(candidate).map_err(|error| {
            format!(
                "cannot canonicalize executable {}: {error}",
                path_display(candidate)
            )
        })?
    } else {
        candidate.to_path_buf()
    };
    let target_metadata = fs::symlink_metadata(&target).map_err(|error| {
        format!(
            "cannot inspect executable target {}: {error}",
            path_display(&target)
        )
    })?;
    if !target_metadata.file_type().is_file() || is_reparse_point(&target_metadata) {
        return Err(format!(
            "{} does not resolve to a regular executable file",
            path_display(candidate)
        ));
    }
    Ok(target)
}

fn program_names(program: &str) -> Vec<OsString> {
    #[cfg(windows)]
    {
        if program.to_ascii_lowercase().ends_with(".exe") {
            vec![OsString::from(program)]
        } else {
            vec![
                OsString::from(format!("{program}.exe")),
                OsString::from(program),
            ]
        }
    }
    #[cfg(not(windows))]
    {
        vec![OsString::from(program)]
    }
}

fn command_failure_detail(observation: &CommandObservation) -> String {
    if let Some(error) = &observation.error {
        return error.clone();
    }
    if observation.exit_code != Some(0) {
        return format!(
            "command exited {:?}: {}",
            observation.exit_code,
            observation.stderr.trim()
        );
    }
    "command output does not match the required JSON identity contract".to_owned()
}

fn run_bounded(program: &str, args: &[&str]) -> CommandObservation {
    let mut command = Command::new(program);
    command.args(args);
    run_bounded_command(command)
}

fn run_bounded_path(program: &Path, args: &[&str]) -> CommandObservation {
    let mut command = Command::new(program);
    command.args(args);
    run_bounded_command(command)
}

fn run_bounded_command(mut command: Command) -> CommandObservation {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        // The doctor must be able to stop descendants that inherited the
        // command process.  A fresh group lets timeout cleanup target only
        // this probe, never the caller's process group.
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return CommandObservation::failure(error.to_string()),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_bounded_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_bounded_pipe(stderr));
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_process_tree(&mut child);
                break child.wait();
            }
            Err(error) => break Err(error),
        }
    };
    let stdout = stdout_reader
        .join()
        .unwrap_or_else(|_| Err("stdout reader panicked".to_owned()));
    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| Err("stderr reader panicked".to_owned()));
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => {
            return CommandObservation::failure(error);
        }
    };
    match status {
        Ok(status) => CommandObservation {
            exit_code: status.code(),
            stdout,
            stderr,
            error: (status.code().is_none() && Instant::now() >= deadline).then_some(format!(
                "command exceeded {} ms",
                COMMAND_TIMEOUT.as_millis()
            )),
        },
        Err(error) => CommandObservation::failure(error.to_string()),
    }
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // A negative PID targets the group created by pre_exec above.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut killer) = taskkill.spawn() {
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if killer.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = killer.kill();
            let _ = killer.wait();
        }
    }
    let _ = child.kill();
}

fn read_bounded_pipe(pipe: Option<impl Read>) -> Result<String, String> {
    let Some(mut pipe) = pipe else {
        return Ok(String::new());
    };
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take((MAX_COMMAND_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_COMMAND_OUTPUT_BYTES {
        return Err(format!(
            "command output exceeds {MAX_COMMAND_OUTPUT_BYTES} bytes"
        ));
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn path_display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Repair with live host seams.  This is the only public mutation authority in
/// this module.  The transaction is application-level all-old/all-new across
/// cache and executable identities; two directory renames are not one OS-wide
/// atomic operation, so the durable journal is required for recovery.
pub fn repair(options: &DiagnoseOptions) -> Result<RepairReport, DoctorError> {
    repair_with(options, &DoctorSeams::system())
}

pub fn repair_with(
    options: &DiagnoseOptions,
    seams: &DoctorSeams,
) -> Result<RepairReport, DoctorError> {
    let initial = diagnose_with(options, seams);
    let root = initial
        .selected_cache_root
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| {
            DoctorError("repair requires one unambiguous selected cache root".to_owned())
        })?;
    if initial.journal.status == "active" {
        recover_interrupted_repair(&root)?;
    }
    let exact_options = DiagnoseOptions::with_plugin_root(&root);
    let before = diagnose_with(&exact_options, seams);
    if !before.identity_healthy {
        return Err(DoctorError(
            "repair requires a healthy current executable/source identity".to_owned(),
        ));
    }
    if before.healthy {
        return Ok(RepairReport {
            repaired: false,
            rolled_back: false,
            selected_cache_root: path_display(&root),
            backup_root: before.journal.backup_root.clone(),
            journal: before.journal.clone(),
            diagnosis_before: before.clone(),
            diagnosis_after: before,
            atomicity_note: atomicity_note(),
        });
    }
    ensure_safe_root(&root).map_err(DoctorError)?;
    let paths = RepairPaths::for_root(&root)?;
    if paths.journal.exists() {
        return Err(DoctorError(format!(
            "active repair journal exists at {}; recover it before a new repair",
            path_display(&paths.journal)
        )));
    }
    let cachebuster = repair_cachebuster(&root).map_err(DoctorError)?;
    copy_tree(&root, &paths.stage).map_err(DoctorError)?;
    if let Err(error) = overwrite_projection(&paths.stage, cachebuster.as_deref()) {
        let preservation = preserve_failed_stage(&paths.stage, &paths.failed);
        return Err(DoctorError(match preservation {
            Ok(()) => format!("staged projection failed: {error}"),
            Err(preservation_error) => format!(
                "staged projection failed: {error}; failed stage could not be preserved: {preservation_error}"
            ),
        }));
    }
    if let Err(error) = projection_exact(&paths.stage) {
        let preservation = preserve_failed_stage(&paths.stage, &paths.failed);
        return Err(DoctorError(match preservation {
            Ok(()) => format!("staged validation failed; original root was retained: {error}"),
            Err(preservation_error) => format!(
                "staged validation failed; original root was retained: {error}; failed stage could not be preserved: {preservation_error}"
            ),
        }));
    }
    let journal = RepairJournal::new(&paths);
    write_journal(&paths.journal, &journal)?;
    fs::rename(&root, &paths.backup)
        .map_err(|error| DoctorError(format!("cannot retain old cache as backup: {error}")))?;
    if let Err(error) = sync_parent(&root) {
        return Err(with_recovery_error(
            format!("cannot sync old-cache backup: {error}"),
            restore_after_old_move(&paths),
        ));
    }
    let old_moved = journal.with_phase("old_moved");
    if let Err(error) = write_journal(&paths.journal, &old_moved) {
        return Err(with_recovery_error(
            format!("cannot persist old-cache move: {error}"),
            restore_after_old_move(&paths),
        ));
    }
    if let Err(error) = fs::rename(&paths.stage, &root) {
        return Err(with_recovery_error(
            format!("cannot promote staged cache: {error}"),
            restore_after_old_move(&paths),
        ));
    }
    if let Err(error) = sync_parent(&root) {
        return Err(with_recovery_error(
            format!("cannot sync promoted cache: {error}"),
            rollback(&paths, &old_moved),
        ));
    }
    if let Err(error) = write_journal(&paths.journal, &journal.with_phase("promoted")) {
        return Err(with_recovery_error(
            format!("cannot persist promoted cache: {error}"),
            rollback(&paths, &old_moved),
        ));
    }

    let after_active = diagnose_with(&exact_options, seams);
    let post_healthy = !after_active.issues.is_empty()
        && after_active
            .issues
            .iter()
            .all(|issue| issue.code == "JOURNAL_ACTIVE");
    if !post_healthy {
        rollback(&paths, &journal)?;
        return Err(DoctorError(
            "post-promotion diagnosis was unhealthy; old cache was restored".to_owned(),
        ));
    }
    remove_journal(&paths.journal)?;
    let after = diagnose_with(&exact_options, seams);
    if !after.healthy {
        rollback_after_journal_removal(&paths)?;
        return Err(DoctorError(
            "post-promotion diagnosis changed after journal removal; old cache was restored"
                .to_owned(),
        ));
    }
    Ok(RepairReport {
        repaired: true,
        rolled_back: false,
        selected_cache_root: path_display(&root),
        backup_root: Some(path_display(&paths.backup)),
        journal: JournalReport {
            path: Some(path_display(&paths.journal)),
            status: "completed".to_owned(),
            phase: Some("verified".to_owned()),
            backup_root: Some(path_display(&paths.backup)),
            detail: None,
        },
        diagnosis_before: before,
        diagnosis_after: after,
        atomicity_note: atomicity_note(),
    })
}

fn atomicity_note() -> String {
    "Repair provides application-level all-old/all-new recovery across the cache and executable identity checks; it is not one OS-wide atomic operation.".to_owned()
}

fn with_recovery_error(primary: String, recovery: Result<(), DoctorError>) -> DoctorError {
    match recovery {
        Ok(()) => DoctorError(format!("{primary}; old cache was restored")),
        Err(error) => DoctorError(format!(
            "{primary}; rollback also failed: {error}. The durable repair journal was retained for recovery"
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepairJournal {
    schema_version: u32,
    root_name: String,
    stage_name: String,
    backup_name: String,
    failed_name: String,
    phase: String,
}

impl RepairJournal {
    fn new(paths: &RepairPaths) -> Self {
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            root_name: paths
                .root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            stage_name: paths
                .stage
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            backup_name: paths
                .backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            failed_name: paths
                .failed
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            phase: "prepared".to_owned(),
        }
    }

    fn with_phase(&self, phase: &str) -> Self {
        let mut next = self.clone();
        next.phase = phase.to_owned();
        next
    }
}

#[derive(Debug, Clone)]
struct RepairPaths {
    root: PathBuf,
    journal: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    failed: PathBuf,
}

impl RepairPaths {
    fn for_root(root: &Path) -> Result<Self, DoctorError> {
        let parent = root
            .parent()
            .ok_or_else(|| DoctorError("cache root has no parent directory".to_owned()))?;
        ensure_safe_parent(parent).map_err(DoctorError)?;
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| DoctorError("cache root has an unsafe file name".to_owned()))?;
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            REPAIR_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            root: root.to_path_buf(),
            journal: parent.join(format!(".{name}{JOURNAL_SUFFIX}")),
            stage: parent.join(format!(".{name}.epic-doctor-stage-{nonce}")),
            backup: parent.join(format!(".{name}.epic-doctor-backup-{nonce}")),
            failed: parent.join(format!(".{name}.epic-doctor-failed-{nonce}")),
        })
    }
}

fn repair_cachebuster(root: &Path) -> Result<Option<String>, String> {
    let expected_plugin = find_artifact(".codex-plugin/plugin.json")
        .ok_or_else(|| "embedded Codex plugin manifest is unavailable".to_owned())?;
    let expected = expected_plugin.expected_bytes(None)?;
    let expected_version = parse_json(&expected, ".codex-plugin/plugin.json")
        .ok()
        .and_then(|value| {
            value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| "embedded Codex plugin manifest has no version".to_owned())?;
    let expected_base = plugin_base_version(&expected_version, ".codex-plugin/plugin.json")?;
    let manifest_version = read_owned_file(root, ".codex-plugin/plugin.json")
        .ok()
        .and_then(|bytes| {
            let value = parse_json(&bytes, ".codex-plugin/plugin.json").ok()?;
            value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let directory_version = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            plugin_base_version(name, "Codex cache directory")
                .ok()
                .map(|base| {
                    (
                        base.to_owned(),
                        name.split_once('+').map(|(_, suffix)| suffix.to_owned()),
                    )
                })
        });
    let manifest = manifest_version.as_deref().and_then(|version| {
        plugin_base_version(version, ".codex-plugin/plugin.json")
            .ok()
            .map(|base| {
                (
                    base.to_owned(),
                    version.split_once('+').map(|(_, suffix)| suffix.to_owned()),
                )
            })
    });
    let Some((base, _)) = manifest.as_ref().or(directory_version.as_ref()) else {
        return Err(
            "installed plugin manifest and cache-directory version provide no valid cache identity"
                .to_owned(),
        );
    };
    if base != expected_base {
        return Err(format!(
            "installed plugin base version {base} differs from executable version {expected_base}"
        ));
    }
    if let (Some((manifest_base, manifest_suffix)), Some((directory_base, directory_suffix))) =
        (&manifest, &directory_version)
    {
        if manifest_base != directory_base {
            return Err(
                "installed plugin manifest and cache-directory base versions disagree".to_owned(),
            );
        }
        if let (Some(manifest_suffix), Some(directory_suffix)) = (manifest_suffix, directory_suffix)
            && manifest_suffix != directory_suffix
        {
            return Err(
                "installed plugin manifest and cache-directory cachebusters disagree".to_owned(),
            );
        }
    }
    Ok(manifest
        .as_ref()
        .and_then(|(_, suffix)| suffix.clone())
        .or_else(|| {
            directory_version
                .as_ref()
                .and_then(|(_, suffix)| suffix.clone())
        }))
}

fn overwrite_projection(stage: &Path, cachebuster: Option<&str>) -> Result<(), String> {
    ensure_safe_root(stage)?;
    for artifact in OWNED_ARTIFACTS {
        let path = owned_path(stage, artifact.path)?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} has no parent", artifact.path))?;
        ensure_safe_parent(parent)?;
        let existing = fs::symlink_metadata(&path).ok();
        if existing
            .is_some_and(|metadata| !metadata.file_type().is_file() || is_reparse_point(&metadata))
        {
            return Err(format!("{} is not a regular owned file", artifact.path));
        }
        let bytes = artifact.expected_bytes(cachebuster)?;
        write_durable_file(&path, &bytes)?;
    }
    sync_parent(stage)?;
    Ok(())
}

fn projection_exact(root: &Path) -> Result<(), String> {
    ensure_safe_root(root)?;
    for artifact in OWNED_ARTIFACTS {
        let bytes = read_owned_file(root, artifact.path).map_err(|error| error.message)?;
        let actual = semantic_digest(&bytes, artifact.mode, artifact.path)?;
        if actual != artifact.expected_digest()? {
            return Err(format!(
                "{} semantic digest differs after staging",
                artifact.path
            ));
        }
    }
    let mut issues = Vec::new();
    validate_installed_manifest(
        Some(
            &read_owned_file(root, "registry/scripts/bundle-manifest.json")
                .map_err(|error| error.message)?,
        ),
        &mut issues,
    );
    if let Some(issue) = issues.first() {
        return Err(format!("{}: {}", issue.code, issue.message));
    }
    Ok(())
}

/// Recover one interrupted repair journal.  Diagnosis deliberately reports an
/// active journal as unhealthy; repair calls this function before it begins a
/// new transaction.
pub fn recover_interrupted_repair(root: &Path) -> Result<RecoveryReport, DoctorError> {
    let paths = RepairPaths::for_root(root)?;
    if !paths.journal.exists() {
        return Ok(RecoveryReport {
            recovered: false,
            action: "no active repair journal".to_owned(),
            journal: JournalReport {
                path: Some(path_display(&paths.journal)),
                status: "none".to_owned(),
                phase: None,
                backup_root: None,
                detail: None,
            },
        });
    }
    let journal = read_journal(&paths.journal, root)?;
    let paths = paths_from_journal(root, &paths.journal, &journal)?;
    let action = match journal.phase.as_str() {
        "prepared" => {
            preserve_failed_stage(&paths.stage, &paths.failed).map_err(DoctorError)?;
            remove_journal(&paths.journal)?;
            "discarded unpromoted stage; old cache remained selected".to_owned()
        }
        "old_moved" => {
            if !paths.root.exists() && paths.backup.exists() {
                fs::rename(&paths.backup, &paths.root)
                    .map_err(|error| DoctorError(format!("cannot restore old cache: {error}")))?;
                sync_parent(&paths.root).map_err(DoctorError)?;
            }
            preserve_failed_stage(&paths.stage, &paths.failed).map_err(DoctorError)?;
            remove_journal(&paths.journal)?;
            "restored old cache after interrupted promotion".to_owned()
        }
        "promoted" => {
            if projection_exact(&paths.root).is_ok() {
                remove_journal(&paths.journal)?;
                "accepted verified promoted cache and retained backup".to_owned()
            } else {
                rollback(&paths, &journal)?;
                "restored old cache because promoted cache did not validate".to_owned()
            }
        }
        "rolled_back" => {
            if !paths.root.exists() {
                return Err(DoctorError(
                    "rolled-back repair journal has no selected old cache root".to_owned(),
                ));
            }
            remove_journal(&paths.journal)?;
            "settled journal after a completed rollback".to_owned()
        }
        other => {
            return Err(DoctorError(format!(
                "repair journal has unsupported phase {other:?}"
            )));
        }
    };
    Ok(RecoveryReport {
        recovered: true,
        action,
        journal: JournalReport {
            path: Some(path_display(&paths.journal)),
            status: "recovered".to_owned(),
            phase: Some(journal.phase),
            backup_root: Some(path_display(&paths.backup)),
            detail: None,
        },
    })
}

fn rollback(paths: &RepairPaths, journal: &RepairJournal) -> Result<(), DoctorError> {
    let failed = paths.failed.clone();
    if paths.root.exists() {
        fs::rename(&paths.root, &failed).map_err(|error| {
            DoctorError(format!("cannot preserve failed promoted cache: {error}"))
        })?;
    }
    if paths.backup.exists() {
        fs::rename(&paths.backup, &paths.root).map_err(|error| {
            DoctorError(format!("cannot restore old cache from backup: {error}"))
        })?;
    } else {
        return Err(DoctorError(
            "repair backup is missing; refusing to invent a replacement".to_owned(),
        ));
    }
    sync_parent(&paths.root).map_err(DoctorError)?;
    let rolled_back = journal.with_phase("rolled_back");
    write_journal(&paths.journal, &rolled_back)?;
    remove_journal(&paths.journal)?;
    Ok(())
}

fn restore_after_old_move(paths: &RepairPaths) -> Result<(), DoctorError> {
    if !paths.root.exists() && paths.backup.exists() {
        fs::rename(&paths.backup, &paths.root).map_err(|error| {
            DoctorError(format!("cannot restore old cache from backup: {error}"))
        })?;
        sync_parent(&paths.root).map_err(DoctorError)?;
    }
    Ok(())
}

fn rollback_after_journal_removal(paths: &RepairPaths) -> Result<(), DoctorError> {
    let synthetic = RepairJournal::new(paths).with_phase("promoted");
    write_journal(&paths.journal, &synthetic)?;
    rollback(paths, &synthetic)
}

fn inspect_journal(root: &Path, issues: &mut Vec<Issue>) -> JournalReport {
    let Ok(paths) = RepairPaths::for_root(root) else {
        return JournalReport {
            path: None,
            status: "unavailable".to_owned(),
            phase: None,
            backup_root: None,
            detail: Some("cannot safely determine journal path".to_owned()),
        };
    };
    if !paths.journal.exists() {
        return JournalReport {
            path: Some(path_display(&paths.journal)),
            status: "none".to_owned(),
            phase: None,
            backup_root: None,
            detail: None,
        };
    }
    match read_journal(&paths.journal, root).and_then(|journal| {
        paths_from_journal(root, &paths.journal, &journal).map(|paths| (journal, paths))
    }) {
        Ok((journal, journal_paths)) => {
            add_issue(
                issues,
                "JOURNAL_ACTIVE",
                "repair journal",
                "an interrupted or active repair requires recovery before ordinary diagnosis is healthy",
                "run codex doctor --repair to recover the journal",
            );
            JournalReport {
                path: Some(path_display(&paths.journal)),
                status: "active".to_owned(),
                phase: Some(journal.phase),
                backup_root: Some(path_display(&journal_paths.backup)),
                detail: None,
            }
        }
        Err(error) => {
            add_issue(
                issues,
                "JOURNAL_INVALID",
                "repair journal",
                error.to_string(),
                "do not repair; inspect the journal beside the selected cache root",
            );
            JournalReport {
                path: Some(path_display(&paths.journal)),
                status: "invalid".to_owned(),
                phase: None,
                backup_root: None,
                detail: Some(error.to_string()),
            }
        }
    }
}

fn read_journal(path: &Path, root: &Path) -> Result<RepairJournal, DoctorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| DoctorError(format!("cannot inspect repair journal: {error}")))?;
    if !metadata.file_type().is_file()
        || is_reparse_point(&metadata)
        || metadata.len() > MAX_ARTIFACT_BYTES as u64
    {
        return Err(DoctorError(
            "repair journal is not a bounded regular file".to_owned(),
        ));
    }
    let bytes = fs::read(path)
        .map_err(|error| DoctorError(format!("cannot read repair journal: {error}")))?;
    let journal = serde_json::from_slice::<RepairJournal>(&bytes)
        .map_err(|error| DoctorError(format!("invalid repair journal JSON: {error}")))?;
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(DoctorError(
            "repair journal schema is unsupported".to_owned(),
        ));
    }
    let root_name = root.file_name().and_then(|name| name.to_str());
    if root_name != Some(journal.root_name.as_str()) {
        return Err(DoctorError(
            "repair journal root does not match selected root".to_owned(),
        ));
    }
    Ok(journal)
}

fn paths_from_journal(
    root: &Path,
    journal_path: &Path,
    journal: &RepairJournal,
) -> Result<RepairPaths, DoctorError> {
    let parent = journal_path
        .parent()
        .ok_or_else(|| DoctorError("repair journal has no parent".to_owned()))?;
    for name in [
        &journal.stage_name,
        &journal.backup_name,
        &journal.failed_name,
    ] {
        if !safe_sibling_name(name) {
            return Err(DoctorError(
                "repair journal contains an unsafe sibling path".to_owned(),
            ));
        }
    }
    Ok(RepairPaths {
        root: root.to_path_buf(),
        journal: journal_path.to_path_buf(),
        stage: parent.join(&journal.stage_name),
        backup: parent.join(&journal.backup_name),
        failed: parent.join(&journal.failed_name),
    })
}

fn safe_sibling_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).components().count() == 1
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn write_journal(path: &Path, journal: &RepairJournal) -> Result<(), DoctorError> {
    let bytes = canonical_json(
        &serde_json::to_value(journal).map_err(|error| DoctorError(error.to_string()))?,
    )
    .map_err(DoctorError)?;
    write_durable_file_new(path, &bytes)
        .or_else(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                write_durable_file(path, &bytes).map_err(io::Error::other)
            } else {
                Err(error)
            }
        })
        .map_err(|error| DoctorError(format!("cannot write repair journal: {error}")))?;
    sync_parent(path).map_err(DoctorError)
}

fn remove_journal(path: &Path) -> Result<(), DoctorError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DoctorError(format!(
            "cannot inspect repair journal for removal: {error}"
        ))
    })?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err(DoctorError(
            "repair journal is not a removable regular file".to_owned(),
        ));
    }
    fs::remove_file(path)
        .map_err(|error| DoctorError(format!("cannot remove repair journal: {error}")))?;
    sync_parent(path).map_err(DoctorError)
}

fn ensure_safe_parent(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path_display(path)))?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err(format!(
            "{} is not a safe non-reparse directory",
            path_display(path)
        ));
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "staging directory already exists: {}",
            path_display(destination)
        ));
    }
    let mut count = 0;
    copy_tree_inner(source, destination, 0, &mut count)?;
    sync_parent(destination)?;
    Ok(())
}

fn copy_tree_inner(
    source: &Path,
    destination: &Path,
    depth: usize,
    count: &mut usize,
) -> Result<(), String> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(format!("directory depth exceeds {MAX_DIRECTORY_DEPTH}"));
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("cannot inspect {}: {error}", path_display(source)))?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err(format!("{} is not a safe directory", path_display(source)));
    }
    fs::create_dir(destination)
        .map_err(|error| format!("cannot create {}: {error}", path_display(destination)))?;
    for entry in fs::read_dir(source)
        .map_err(|error| format!("cannot scan {}: {error}", path_display(source)))?
    {
        *count += 1;
        if *count > MAX_DIRECTORY_ENTRIES {
            return Err(format!(
                "cache scan exceeds {MAX_DIRECTORY_ENTRIES} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("cannot inspect {}: {error}", path_display(&source_path)))?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(format!(
                "refusing symlink or reparse point {}",
                path_display(&source_path)
            ));
        }
        if metadata.file_type().is_dir() {
            copy_tree_inner(&source_path, &destination_path, depth + 1, count)?;
        } else if metadata.file_type().is_file() {
            if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
                return Err(format!(
                    "refusing oversized cache file {}",
                    path_display(&source_path)
                ));
            }
            fs::copy(&source_path, &destination_path)
                .map_err(|error| format!("cannot copy {}: {error}", path_display(&source_path)))?;
            File::open(&destination_path)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    format!("cannot sync {}: {error}", path_display(&destination_path))
                })?;
        } else {
            return Err(format!(
                "refusing non-regular cache entry {}",
                path_display(&source_path)
            ));
        }
    }
    sync_directory(destination)
}

fn write_durable_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn write_durable_file_new(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_parent(path: &Path) -> Result<(), String> {
    path.parent()
        .ok_or_else(|| "path has no parent".to_owned())
        .and_then(sync_directory)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync directory {}: {error}", path_display(path)))
}

fn preserve_failed_stage(stage: &Path, failed: &Path) -> Result<(), String> {
    if !stage.exists() {
        return Ok(());
    }
    if failed.exists() {
        return Err(format!(
            "cannot preserve {} because {} already exists",
            path_display(stage),
            path_display(failed)
        ));
    }
    fs::rename(stage, failed).map_err(|error| {
        format!(
            "cannot rename {} to {}: {error}",
            path_display(stage),
            path_display(failed)
        )
    })?;
    sync_parent(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn identity_json(identity: &BundleIdentity) -> String {
        serde_json::json!({
            "release_version": identity.release_version,
            "runtime_revision": identity.runtime_revision,
            "build_identity": identity.build_identity,
        })
        .to_string()
    }

    fn write_projection(root: &Path, cachebuster: Option<&str>) {
        fs::create_dir_all(root).unwrap();
        for artifact in OWNED_ARTIFACTS {
            let path = root.join(artifact.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, artifact.expected_bytes(cachebuster).unwrap()).unwrap();
        }
    }

    fn fixture() -> (TempDir, PathBuf, DoctorSeams) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("0.8.3+codex.fixture");
        write_projection(&root, Some("codex.fixture"));
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let current = bin.join("current-epic-harness");
        let epic = bin.join(if cfg!(windows) {
            "epic-harness.exe"
        } else {
            "epic-harness"
        });
        let node = bin.join(if cfg!(windows) { "node.exe" } else { "node" });
        fs::write(&current, b"fixture").unwrap();
        fs::write(&epic, b"fixture").unwrap();
        fs::write(&node, b"fixture").unwrap();
        let identity = compiled_identity();
        let mut executable_results = BTreeMap::new();
        executable_results.insert(
            current.clone(),
            CommandObservation::success(identity_json(&identity)),
        );
        executable_results.insert(epic, CommandObservation::success(identity_json(&identity)));
        let home_dir = temp.path().to_path_buf();
        (
            temp,
            root,
            DoctorSeams {
                environment: BTreeMap::new(),
                home_dir: Some(home_dir),
                codex_plugin_list: Some(CommandObservation::failure("not used")),
                path_entries: vec![bin],
                current_executable: Some(current),
                executable_results,
                live_commands: false,
            },
        )
    }

    #[test]
    fn diagnosis_is_read_only_and_reports_nine_derived_chains() {
        let (_temp, root, seams) = fixture();
        let before = fs::read(root.join(".codex-plugin/plugin.json")).unwrap();
        let report = diagnose_with(&DiagnoseOptions::with_plugin_root(&root), &seams);
        assert!(report.healthy, "{}", report.render_human());
        assert_eq!(report.manifest_chains.len(), 9);
        assert_eq!(
            report
                .manifest_chains
                .iter()
                .filter(|chain| chain.subcommand == "guard")
                .count(),
            2
        );
        assert_eq!(
            before,
            fs::read(root.join(".codex-plugin/plugin.json")).unwrap()
        );
        assert!(!RepairPaths::for_root(&root).unwrap().journal.exists());
    }

    #[test]
    fn one_byte_drift_names_the_owned_component() {
        let (_temp, root, seams) = fixture();
        let path = root.join("registry/scripts/install.js");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let report = diagnose_with(&DiagnoseOptions::with_plugin_root(&root), &seams);
        assert!(!report.healthy);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "ARTIFACT_MISMATCH" && issue.component == "registry/scripts/install.js"
        }));
    }

    #[test]
    fn repair_preserves_unowned_files_fixes_projection_and_is_idempotent() {
        let (_temp, root, seams) = fixture();
        fs::write(root.join("unowned.txt"), b"keep me").unwrap();
        fs::write(root.join("mcp_config.json"), b"{\"mcpServers\":{}}").unwrap();
        let options = DiagnoseOptions::with_plugin_root(&root);
        let repaired = repair_with(&options, &seams).unwrap();
        assert!(repaired.repaired, "{:?}", repaired.diagnosis_after.issues);
        assert!(repaired.diagnosis_after.healthy);
        assert_eq!(fs::read(root.join("unowned.txt")).unwrap(), b"keep me");
        assert_eq!(
            plugin_cachebuster(
                &fs::read(root.join(".codex-plugin/plugin.json")).unwrap(),
                ".codex-plugin/plugin.json"
            )
            .unwrap()
            .as_deref(),
            Some("codex.fixture")
        );
        let repeated = repair_with(&options, &seams).unwrap();
        assert!(!repeated.repaired);
        assert!(repeated.diagnosis_after.healthy);
    }

    #[test]
    fn post_promotion_executable_mismatch_rolls_back_old_cache() {
        let (_temp, root, mut seams) = fixture();
        let drift = b"{\"mcpServers\":{}}";
        fs::write(root.join("mcp_config.json"), drift).unwrap();
        let current = seams.current_executable.clone().unwrap();
        let hook_runtime = seams
            .executable_results
            .keys()
            .find(|path| path.as_path() != current.as_path())
            .cloned()
            .unwrap();
        let mut stale = compiled_identity();
        stale.build_identity =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        seams.executable_results.insert(
            hook_runtime,
            CommandObservation::success(identity_json(&stale)),
        );
        let error = repair_with(&DiagnoseOptions::with_plugin_root(&root), &seams).unwrap_err();
        assert!(error.to_string().contains("post-promotion diagnosis"));
        assert_eq!(fs::read(root.join("mcp_config.json")).unwrap(), drift);
        assert!(!RepairPaths::for_root(&root).unwrap().journal.exists());
    }

    #[test]
    fn ambiguous_active_codex_discovery_never_selects_a_candidate() {
        let (temp, _root, mut seams) = fixture();
        seams.codex_plugin_list = Some(CommandObservation::success(serde_json::json!({
            "plugins": [
                {"pluginId":"epic@personal","name":"epic","marketplaceName":"personal","version":"0.8.3+codex.a","installed":true,"enabled":true},
                {"pluginId":"epic@personal","name":"epic","marketplaceName":"personal","version":"0.8.3+codex.b","installed":true,"enabled":true}
            ]
        }).to_string()));
        let report = diagnose_with(&DiagnoseOptions::default(), &seams);
        assert!(report.selected_cache_root.is_none());
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "DISCOVERY_AMBIGUOUS")
        );
        drop(temp);
    }

    #[test]
    fn split_hook_and_mcp_executable_authority_is_named() {
        let (_temp, root, mut seams) = fixture();
        let current = seams.current_executable.clone().unwrap();
        let hook_runtime = seams
            .executable_results
            .keys()
            .find(|path| path.as_path() != current.as_path())
            .cloned()
            .unwrap();
        let mut split = compiled_identity();
        split.runtime_revision = "999".to_owned();
        seams.executable_results.insert(
            hook_runtime,
            CommandObservation::success(identity_json(&split)),
        );
        let report = diagnose_with(&DiagnoseOptions::with_plugin_root(&root), &seams);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "EXECUTABLE_AUTHORITY_SPLIT")
        );
    }

    #[test]
    fn active_valid_journal_makes_ordinary_diagnosis_unhealthy() {
        let (_temp, root, seams) = fixture();
        let paths = RepairPaths::for_root(&root).unwrap();
        write_journal(&paths.journal, &RepairJournal::new(&paths)).unwrap();
        let report = diagnose_with(&DiagnoseOptions::with_plugin_root(&root), &seams);
        assert!(!report.healthy);
        assert_eq!(report.journal.status, "active");
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "JOURNAL_ACTIVE")
        );
    }

    #[test]
    fn traversal_journal_is_rejected_without_touching_root_or_outside() {
        let (temp, root, _seams) = fixture();
        let paths = RepairPaths::for_root(&root).unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        let mut journal = RepairJournal::new(&paths);
        journal.stage_name = "../outside".to_owned();
        write_journal(&paths.journal, &journal).unwrap();
        assert!(recover_interrupted_repair(&root).is_err());
        assert!(root.exists());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn interrupted_old_move_recovers_old_root() {
        let (_temp, root, _seams) = fixture();
        let paths = RepairPaths::for_root(&root).unwrap();
        let journal = RepairJournal::new(&paths).with_phase("old_moved");
        fs::rename(&root, &paths.backup).unwrap();
        write_journal(&paths.journal, &journal).unwrap();
        let recovered = recover_interrupted_repair(&root).unwrap();
        assert!(recovered.recovered);
        assert!(root.exists());
        assert!(!paths.journal.exists());
    }

    #[test]
    fn recovery_keeps_journal_when_failed_stage_cannot_be_preserved() {
        let (_temp, root, _seams) = fixture();
        let paths = RepairPaths::for_root(&root).unwrap();
        fs::create_dir_all(&paths.stage).unwrap();
        fs::create_dir_all(&paths.failed).unwrap();
        write_journal(&paths.journal, &RepairJournal::new(&paths)).unwrap();

        let error = recover_interrupted_repair(&root).unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert!(paths.stage.exists(), "the unresolved stage remains visible");
        assert!(
            paths.journal.exists(),
            "recovery must not settle the journal after preservation fails"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_symlink_is_recorded_and_its_regular_target_is_probed() {
        use std::os::unix::fs::symlink;

        let (temp, root, mut seams) = fixture();
        let link = seams.path_entries.first().unwrap().join("epic-harness");
        let target = temp.path().join("homebrew-real-epic-harness");
        fs::write(&target, b"fixture").unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&target, &link).unwrap();
        seams.executable_results.remove(&link);
        seams.executable_results.insert(
            fs::canonicalize(&target).unwrap(),
            CommandObservation::success(identity_json(&compiled_identity())),
        );

        let report = diagnose_with(&DiagnoseOptions::with_plugin_root(&root), &seams);
        assert!(report.healthy, "{}", report.render_human());
        let hook_runtime = report
            .commands
            .executables
            .iter()
            .find(|entry| entry.role == "hook_runtime")
            .unwrap();
        let target_display = path_display(&fs::canonicalize(&target).unwrap());
        assert_eq!(hook_runtime.all_path_matches, vec![path_display(&link)]);
        assert_eq!(
            hook_runtime.resolved_executable.as_deref(),
            Some(target_display.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn repair_rejects_symlinked_unowned_tree_without_touching_old_root() {
        use std::os::unix::fs::symlink;

        let (temp, root, seams) = fixture();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, root.join("unowned-link")).unwrap();
        fs::write(root.join("mcp_config.json"), b"{\"mcpServers\":{}}").unwrap();
        let original = fs::read(root.join("mcp_config.json")).unwrap();
        let error = repair_with(&DiagnoseOptions::with_plugin_root(&root), &seams).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(original, fs::read(root.join("mcp_config.json")).unwrap());
    }
}
