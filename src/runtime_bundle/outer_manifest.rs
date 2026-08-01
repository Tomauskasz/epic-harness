//! Strict loading for a post-link runtime-bundle manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Candidate, TransactionError};

/// The outer manifest is intentionally small.  It must remain bounded before
/// JSON parsing because it is supplied alongside an untrusted downloaded
/// bundle.
pub const MAX_OUTER_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_MATERIALIZED_FILES: usize = 4_096;
pub const MAX_SINGLE_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_CANDIDATE_BYTES: u64 = 256 * 1024 * 1024;

const OUTER_SCHEMA_VERSION: u32 = 1;
const OUTER_MANIFEST_KIND: &str = "post-link-runtime-bundle-v1";
const OUTER_BUNDLE_ID_ALGORITHM: &str = "sha256-canonical-post-link-runtime-bundle-v1";
const OUTER_BUNDLE_ID_DOMAIN: &[u8] = b"epic-harness-post-link-runtime-bundle\0v1\0";
const SELECTOR_PROTOCOL: &str = "epic-harness-materialized-runtime-v1";
const TARGET_EXECUTABLE_SELECTOR: &str = "target-executable-v1";
const RAW_BYTES_DIGEST_MODE: &str = "raw-bytes-v1";
const LOGICAL_MANIFEST_PATH: &str = "registry/scripts/bundle-manifest.json";
const LOGICAL_MANIFEST_KIND: &str = "logical-runtime-v1";
const LOGICAL_IDENTITY_ALGORITHM: &str = "sha256-framed-logical-source-and-artifact-projection-v3";

/// Locations and target identity required to load one post-link bundle.
///
/// The outer manifest may be outside the materialized root.  Release tooling
/// deliberately supports that layout, so only materialized payload paths are
/// required to be contained by `materialized_root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateLoadOptions {
    pub materialized_root: PathBuf,
    pub outer_manifest: PathBuf,
    pub expected_target: String,
}

impl CandidateLoadOptions {
    pub fn new(
        materialized_root: impl Into<PathBuf>,
        outer_manifest: impl Into<PathBuf>,
        expected_target: impl Into<String>,
    ) -> Self {
        Self {
            materialized_root: materialized_root.into(),
            outer_manifest: outer_manifest.into(),
            expected_target: expected_target.into(),
        }
    }
}

/// A materialized regular file committed by the post-link manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedFile {
    pub path: String,
    #[serde(rename = "type")]
    pub file_type: String,
    pub mode: String,
    pub size: u64,
    pub digest_mode: String,
    pub sha256: String,
}

/// The one executable selected by the post-link manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OuterExecutable {
    pub selector: String,
    pub path: String,
    #[serde(rename = "type")]
    pub file_type: String,
    pub mode: String,
    pub size: u64,
    pub digest_mode: String,
    pub sha256: String,
}

impl OuterExecutable {
    fn materialized_file(&self) -> MaterializedFile {
        MaterializedFile {
            path: self.path.clone(),
            file_type: self.file_type.clone(),
            mode: self.mode.clone(),
            size: self.size,
            digest_mode: self.digest_mode.clone(),
            sha256: self.sha256.clone(),
        }
    }
}

/// The complete post-link protocol-v1 manifest.  `deny_unknown_fields` is
/// deliberate: a different protocol must fail closed rather than be accepted
/// as an extension of v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OuterManifest {
    pub schema_version: u32,
    pub manifest_kind: String,
    pub bundle_id_algorithm: String,
    pub selector_protocol: String,
    pub logical_build_identity: String,
    pub target_triple: String,
    pub executable: OuterExecutable,
    pub materialized_files: Vec<MaterializedFile>,
    pub bundle_id: String,
}

/// A fully checked candidate plus the evidence retained for diagnosis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedCandidate {
    pub candidate: Candidate,
    pub manifest: OuterManifest,
}

/// Load and validate a complete post-link bundle before the transaction core
/// can acquire a repair lock or mutate an active selector.
pub fn load_candidate(options: &CandidateLoadOptions) -> Result<LoadedCandidate, TransactionError> {
    validate_target(&options.expected_target)?;
    ensure_real_directory(&options.materialized_root, "materialized runtime root")?;

    let outer_bytes = read_regular_file(
        &options.outer_manifest,
        "outer manifest",
        MAX_OUTER_MANIFEST_BYTES,
    )?;
    let outer_value: Value = serde_json::from_slice(&outer_bytes).map_err(|error| {
        invalid(format!(
            "outer manifest is not valid JSON {}: {error}",
            options.outer_manifest.display()
        ))
    })?;
    let manifest: OuterManifest = serde_json::from_value(outer_value.clone()).map_err(|error| {
        invalid(format!(
            "outer manifest does not exactly implement protocol v1: {error}"
        ))
    })?;

    validate_outer_manifest(&manifest, &outer_value, &options.expected_target)?;
    let executable_descriptor = manifest.executable.materialized_file();
    let executable = read_and_verify_materialized(
        &options.materialized_root,
        &executable_descriptor,
        "target executable",
    )?;

    let mut files = BTreeMap::new();
    let mut total_bytes = executable.len() as u64;
    for file in &manifest.materialized_files {
        let bytes = read_and_verify_materialized(
            &options.materialized_root,
            file,
            "materialized runtime file",
        )?;
        total_bytes = total_bytes.checked_add(bytes.len() as u64).ok_or_else(|| {
            invalid("post-link bundle byte count overflows its bounded input accounting")
        })?;
        if total_bytes > MAX_CANDIDATE_BYTES {
            return Err(invalid(format!(
                "post-link bundle exceeds {MAX_CANDIDATE_BYTES} byte candidate limit"
            )));
        }
        if files.insert(file.path.clone(), bytes).is_some() {
            return Err(invalid(format!(
                "outer manifest has duplicate materialized path {}",
                file.path
            )));
        }
    }

    validate_logical_projection(&manifest, &files)?;
    Ok(LoadedCandidate {
        candidate: Candidate {
            id: manifest.bundle_id.clone(),
            executable,
            files,
        },
        manifest,
    })
}

fn validate_outer_manifest(
    manifest: &OuterManifest,
    outer_value: &Value,
    expected_target: &str,
) -> Result<(), TransactionError> {
    if manifest.schema_version != OUTER_SCHEMA_VERSION {
        return Err(invalid(format!(
            "outer manifest schema_version is {}, expected {OUTER_SCHEMA_VERSION}",
            manifest.schema_version
        )));
    }
    exact(
        &manifest.manifest_kind,
        OUTER_MANIFEST_KIND,
        "outer manifest kind",
    )?;
    exact(
        &manifest.bundle_id_algorithm,
        OUTER_BUNDLE_ID_ALGORITHM,
        "outer bundle identity algorithm",
    )?;
    exact(
        &manifest.selector_protocol,
        SELECTOR_PROTOCOL,
        "outer selector protocol",
    )?;
    if manifest.target_triple != expected_target {
        return Err(invalid(format!(
            "outer manifest target {} does not match expected target {expected_target}",
            manifest.target_triple
        )));
    }
    validate_sha256(
        &manifest.logical_build_identity,
        "outer logical build identity",
    )?;
    validate_sha256(&manifest.bundle_id, "outer bundle id")?;
    exact(
        &manifest.executable.selector,
        TARGET_EXECUTABLE_SELECTOR,
        "outer executable selector",
    )?;
    let executable = manifest.executable.materialized_file();
    validate_materialized_descriptor(&executable, "outer executable")?;

    if manifest.materialized_files.is_empty() {
        return Err(invalid("outer manifest has no materialized files"));
    }
    if manifest.materialized_files.len() > MAX_MATERIALIZED_FILES {
        return Err(invalid(format!(
            "outer manifest has more than {MAX_MATERIALIZED_FILES} materialized files"
        )));
    }
    let mut paths = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for file in &manifest.materialized_files {
        validate_materialized_descriptor(file, "outer materialized file")?;
        if file.path == executable.path {
            return Err(invalid(format!(
                "outer executable path {} is also a materialized file",
                file.path
            )));
        }
        if !paths.insert(file.path.as_str()) {
            return Err(invalid(format!(
                "outer manifest has duplicate materialized path {}",
                file.path
            )));
        }
        if previous.is_some_and(|prior| prior >= file.path.as_str()) {
            return Err(invalid(
                "outer materialized files are not in the protocol-v1 canonical path order",
            ));
        }
        previous = Some(&file.path);
    }

    let mut without_bundle_id = outer_value.clone();
    let object = without_bundle_id.as_object_mut().ok_or_else(|| {
        invalid("outer manifest must be a JSON object before bundle-id verification")
    })?;
    object
        .remove("bundle_id")
        .ok_or_else(|| invalid("outer manifest has no bundle_id before bundle-id verification"))?;
    let canonical = canonical_json(&without_bundle_id)
        .map_err(|error| invalid(format!("cannot canonicalize outer manifest: {error}")))?;
    let mut digest = Sha256::new();
    digest.update(OUTER_BUNDLE_ID_DOMAIN);
    digest.update(canonical);
    let expected_bundle_id = format!("sha256:{:x}", digest.finalize());
    if manifest.bundle_id != expected_bundle_id {
        return Err(invalid(format!(
            "outer bundle_id does not match its canonical protocol-v1 manifest: expected {expected_bundle_id}"
        )));
    }
    Ok(())
}

fn validate_materialized_descriptor(
    descriptor: &MaterializedFile,
    label: &str,
) -> Result<(), TransactionError> {
    validate_portable_path(&descriptor.path, &format!("{label} path"))?;
    exact(&descriptor.file_type, "file", &format!("{label} type"))?;
    exact(
        &descriptor.digest_mode,
        RAW_BYTES_DIGEST_MODE,
        &format!("{label} digest mode"),
    )?;
    parse_mode(&descriptor.mode, &format!("{label} mode"))?;
    if descriptor.size > MAX_SINGLE_FILE_BYTES {
        return Err(invalid(format!(
            "{label} {} exceeds {MAX_SINGLE_FILE_BYTES} byte file limit",
            descriptor.path
        )));
    }
    validate_sha256(&descriptor.sha256, &format!("{label} sha256"))
}

fn read_and_verify_materialized(
    root: &Path,
    descriptor: &MaterializedFile,
    label: &str,
) -> Result<Vec<u8>, TransactionError> {
    let path = contained_materialized_path(root, &descriptor.path, label)?;
    let bytes = read_regular_file(&path, label, MAX_SINGLE_FILE_BYTES)?;
    if bytes.len() as u64 != descriptor.size {
        return Err(invalid(format!(
            "{label} {} size is {}, expected {}",
            descriptor.path,
            bytes.len(),
            descriptor.size
        )));
    }
    verify_mode(&path, &descriptor.mode, label)?;
    let actual = sha256(&bytes);
    if actual != descriptor.sha256 {
        return Err(invalid(format!(
            "{label} {} digest differs from its outer manifest",
            descriptor.path
        )));
    }
    Ok(bytes)
}

fn validate_logical_projection(
    outer: &OuterManifest,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), TransactionError> {
    let logical_bytes = files.get(LOGICAL_MANIFEST_PATH).ok_or_else(|| {
        invalid(format!(
            "outer materialized file list omits required logical manifest {LOGICAL_MANIFEST_PATH}"
        ))
    })?;
    let logical: LogicalManifest = serde_json::from_slice(logical_bytes).map_err(|error| {
        invalid(format!(
            "materialized logical manifest {LOGICAL_MANIFEST_PATH} is invalid: {error}"
        ))
    })?;
    if logical.schema_version != 1 || logical.manifest_kind != LOGICAL_MANIFEST_KIND {
        return Err(invalid(
            "materialized logical manifest has an unsupported schema or kind",
        ));
    }
    validate_sha256(
        &logical.build_identity,
        "materialized logical build identity",
    )?;
    if logical.build_identity != outer.logical_build_identity {
        return Err(invalid(
            "outer logical build identity does not match the materialized logical manifest",
        ));
    }
    if logical.identity_inputs.algorithm != LOGICAL_IDENTITY_ALGORITHM
        || logical.identity_inputs.inventory_path != "registry/scripts/bundle-spec.json"
    {
        return Err(invalid(
            "materialized logical manifest has an unsupported identity input protocol",
        ));
    }

    let mut expected = BTreeSet::from([LOGICAL_MANIFEST_PATH.to_owned()]);
    let mut artifact_paths = Vec::with_capacity(logical.artifacts.len());
    for artifact in &logical.artifacts {
        validate_portable_path(&artifact.path, "logical artifact path")?;
        exact(&artifact.file_type, "file", "logical artifact type")?;
        if !matches!(
            artifact.digest_mode.as_str(),
            "canonical-plugin-json-v1" | "canonical-json-v1" | "normalized-lf-text-v1"
        ) {
            return Err(invalid(format!(
                "logical artifact {} has unsupported digest mode {}",
                artifact.path, artifact.digest_mode
            )));
        }
        validate_sha256(&artifact.sha256, "logical artifact sha256")?;
        if !expected.insert(artifact.path.clone()) {
            return Err(invalid(format!(
                "materialized logical manifest has a duplicate artifact path {}",
                artifact.path
            )));
        }
        artifact_paths.push(artifact.path.clone());
    }
    if logical.identity_inputs.artifact_paths != artifact_paths {
        return Err(invalid(
            "materialized logical manifest identity artifact path projection differs from artifacts",
        ));
    }
    let actual = files.keys().cloned().collect::<BTreeSet<_>>();
    if actual != expected {
        let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
        let extra = actual.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(invalid(format!(
            "outer materialized paths differ from the logical projection; missing={missing:?}, extra={extra:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalManifest {
    schema_version: u32,
    manifest_kind: String,
    #[allow(dead_code)]
    release_version: String,
    #[allow(dead_code)]
    runtime_revision: String,
    build_identity: String,
    artifacts: Vec<LogicalArtifact>,
    identity_inputs: LogicalIdentityInputs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalArtifact {
    path: String,
    #[serde(rename = "type")]
    file_type: String,
    digest_mode: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalIdentityInputs {
    algorithm: String,
    inventory_path: String,
    #[allow(dead_code)]
    source_paths: Vec<String>,
    artifact_paths: Vec<String>,
}

fn contained_materialized_path(
    root: &Path,
    relative: &str,
    label: &str,
) -> Result<PathBuf, TransactionError> {
    validate_portable_path(relative, &format!("{label} path"))?;
    ensure_real_directory(root, "materialized runtime root")?;
    let mut path = root.to_path_buf();
    for (index, component) in relative.split('/').enumerate() {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            invalid(format!(
                "cannot inspect {label} {}: {error}",
                path.display()
            ))
        })?;
        let final_component = index + 1 == relative.split('/').count();
        if final_component {
            ensure_real_file_metadata(&metadata, label, &path)?;
        } else if !is_real_directory(&metadata) {
            return Err(invalid(format!(
                "{label} parent is not a regular non-symlink directory: {}",
                path.display()
            )));
        }
    }
    let canonical_root = crate::shared::paths::canonical_for_compare(root).map_err(|error| {
        invalid(format!(
            "cannot canonicalize materialized runtime root {}: {error}",
            root.display()
        ))
    })?;
    let canonical_path = crate::shared::paths::canonical_for_compare(&path).map_err(|error| {
        invalid(format!(
            "cannot canonicalize {label} {}: {error}",
            path.display()
        ))
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(invalid(format!(
            "{label} escapes materialized runtime root: {relative}"
        )));
    }
    Ok(path)
}

fn read_regular_file(path: &Path, label: &str, limit: u64) -> Result<Vec<u8>, TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        invalid(format!(
            "cannot inspect {label} {}: {error}",
            path.display()
        ))
    })?;
    ensure_real_file_metadata(&metadata, label, path)?;
    if metadata.len() > limit {
        return Err(invalid(format!(
            "{label} {} exceeds {limit} byte input limit",
            path.display()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|error| invalid(format!("cannot read {label} {}: {error}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(invalid(format!(
            "{label} {} exceeds {limit} byte input limit after read",
            path.display()
        )));
    }
    Ok(bytes)
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<(), TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        invalid(format!(
            "cannot inspect {label} {}: {error}",
            path.display()
        ))
    })?;
    if !is_real_directory(&metadata) {
        return Err(invalid(format!(
            "{label} must be a regular non-symlink directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_real_file_metadata(
    metadata: &Metadata,
    label: &str,
    path: &Path,
) -> Result<(), TransactionError> {
    if metadata.file_type().is_symlink() || is_reparse_point(metadata) || !metadata.is_file() {
        return Err(invalid(format!(
            "{label} must be a regular non-symlink file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn is_real_directory(metadata: &Metadata) -> bool {
    !metadata.file_type().is_symlink() && !is_reparse_point(metadata) && metadata.is_dir()
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

fn verify_mode(path: &Path, expected: &str, label: &str) -> Result<(), TransactionError> {
    let expected = parse_mode(expected, &format!("{label} mode"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let actual = fs::symlink_metadata(path)
            .map_err(|error| {
                invalid(format!(
                    "cannot inspect {label} {}: {error}",
                    path.display()
                ))
            })?
            .mode()
            & 0o777;
        if actual != expected {
            return Err(invalid(format!(
                "{label} {} mode is {actual:04o}, expected {expected:04o}",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows does not expose POSIX permission bits. The descriptor still
        // has a strictly checked v1 spelling, while the byte, type, and size
        // contracts remain enforceable on that host.
        let _ = (path, expected);
    }
    Ok(())
}

fn parse_mode(value: &str, label: &str) -> Result<u32, TransactionError> {
    let valid = value.len() == 4
        && value.starts_with('0')
        && value.as_bytes()[1..]
            .iter()
            .all(|byte| (b'0'..=b'7').contains(byte));
    if !valid {
        return Err(invalid(format!(
            "{label} must be an octal 0xxx mode: {value}"
        )));
    }
    u32::from_str_radix(&value[1..], 8)
        .map_err(|error| invalid(format!("{label} has invalid octal mode {value}: {error}")))
}

fn validate_target(target: &str) -> Result<(), TransactionError> {
    if target.is_empty()
        || target.len() > 255
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid(
            "expected target must be a nonempty target-triple token",
        ));
    }
    Ok(())
}

fn validate_portable_path(path: &str, label: &str) -> Result<(), TransactionError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || Path::new(path).is_absolute()
    {
        return Err(invalid(format!(
            "{label} must be a nonempty portable relative path: {path}"
        )));
    }
    if path.len() > 4_096 {
        return Err(invalid(format!("{label} exceeds the 4096 byte path limit")));
    }
    if path.split('/').any(|component| {
        component.is_empty()
            || matches!(component, "." | "..")
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }) {
        return Err(invalid(format!(
            "{label} has traversal or unsupported components: {path}"
        )));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<(), TransactionError> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid(format!("{label} must use a sha256: prefix")))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "{label} must be a lower-case sha256 digest"
        )));
    }
    Ok(())
}

fn exact(actual: &str, expected: &str, label: &str) -> Result<(), TransactionError> {
    (actual == expected)
        .then_some(())
        .ok_or_else(|| invalid(format!("{label} is {actual:?}, expected {expected:?}")))
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Protocol-local canonical JSON.  The post-link manifest defines its bundle
/// id over recursively sorted object keys, so this must not borrow a host
/// adapter's JSON policy.
pub(crate) fn canonical_json(value: &Value) -> Result<Vec<u8>, String> {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(values.iter().map(sort).collect()),
            Value::Object(values) => Value::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), sort(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            primitive => primitive.clone(),
        }
    }
    serde_json::to_vec(&sort(value)).map_err(|error| format!("cannot canonicalize JSON: {error}"))
}

fn invalid(message: impl Into<String>) -> TransactionError {
    TransactionError::InvalidCandidate(message.into())
}

#[cfg(test)]
mod tests {
    use super::{
        CandidateLoadOptions, LOGICAL_MANIFEST_PATH, OUTER_BUNDLE_ID_DOMAIN, load_candidate,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    struct Fixture {
        root: tempfile::TempDir,
        outer: PathBuf,
    }

    impl Fixture {
        fn load(&self) -> Result<super::LoadedCandidate, super::TransactionError> {
            load_candidate(&CandidateLoadOptions::new(
                self.root.path(),
                &self.outer,
                TARGET,
            ))
        }
    }

    #[test]
    fn loads_a_complete_post_link_bundle_for_the_exact_target() {
        let fixture = fixture();
        let loaded = fixture.load().expect("complete fixture must load");
        assert_eq!(loaded.candidate.executable, b"runtime executable");
        assert_eq!(loaded.candidate.id, loaded.manifest.bundle_id);
        assert_eq!(loaded.manifest.target_triple, TARGET);
        assert_eq!(
            loaded.candidate.files.get("hooks/hooks.json"),
            Some(&b"{\"hooks\":[]}".to_vec())
        );
        assert!(loaded.candidate.files.contains_key(LOGICAL_MANIFEST_PATH));
    }

    #[test]
    fn malformed_outer_manifest_fails_at_the_candidate_boundary() {
        let temporary = tempfile::tempdir().expect("temporary runtime root");
        let outer = temporary.path().join("outer.json");
        fs::write(&outer, "{}").expect("outer manifest");
        let error = load_candidate(&CandidateLoadOptions::new(temporary.path(), &outer, TARGET))
            .expect_err("an incomplete outer manifest must be rejected");
        assert!(
            error.to_string().contains("outer manifest"),
            "the candidate seam must report the manifest boundary: {error}"
        );
    }

    #[test]
    fn wrong_target_and_corrupt_payload_are_rejected_before_a_candidate_exists() {
        let fixture = fixture();
        let wrong_target = load_candidate(&CandidateLoadOptions::new(
            fixture.root.path(),
            &fixture.outer,
            "aarch64-apple-darwin",
        ));
        assert!(
            wrong_target.is_err(),
            "target mismatch must reject the candidate"
        );

        fs::write(
            fixture.root.path().join("bin/epic-harness"),
            b"corrupt executable",
        )
        .expect("corrupt fixture executable");
        let error = fixture
            .load()
            .expect_err("a raw executable digest mismatch must reject the candidate");
        assert!(error.to_string().contains("digest"), "{error}");

        let fixture = fixture();
        fs::write(
            fixture.root.path().join("hooks/hooks.json"),
            b"{\"hooks\":[\"corrupt\"]}",
        )
        .expect("corrupt fixture materialized file");
        let error = fixture
            .load()
            .expect_err("a materialized file digest mismatch must reject the candidate");
        assert!(error.to_string().contains("digest"), "{error}");
    }

    #[test]
    fn rejects_extra_duplicate_and_traversal_materialized_paths() {
        let fixture = fixture();
        let mut outer: Value = serde_json::from_slice(&fs::read(&fixture.outer).unwrap()).unwrap();
        let files = outer["materialized_files"].as_array_mut().unwrap();
        files[1]["path"] = json!("../outside");
        set_bundle_id(&mut outer);
        fs::write(&fixture.outer, serde_json::to_vec(&outer).unwrap()).unwrap();
        let error = fixture.load().expect_err("traversal path must reject");
        assert!(error.to_string().contains("traversal"), "{error}");

        let fixture = fixture();
        let mut outer: Value = serde_json::from_slice(&fs::read(&fixture.outer).unwrap()).unwrap();
        let duplicate = outer["materialized_files"][0].clone();
        outer["materialized_files"].as_array_mut().unwrap()[1] = duplicate;
        set_bundle_id(&mut outer);
        fs::write(&fixture.outer, serde_json::to_vec(&outer).unwrap()).unwrap();
        let error = fixture.load().expect_err("duplicate path must reject");
        assert!(error.to_string().contains("duplicate"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_materialized_file() {
        let fixture = fixture();
        let outside = fixture.root.path().join("outside.json");
        fs::write(&outside, b"{\"hooks\":[]}").unwrap();
        let target = fixture.root.path().join("hooks/hooks.json");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&outside, &target).unwrap();

        let error = fixture.load().expect_err("symlink payload must reject");
        assert!(error.to_string().contains("regular non-symlink"), "{error}");
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("temporary runtime root");
        write(root.path(), "bin/epic-harness", b"runtime executable");
        write(root.path(), "hooks/hooks.json", b"{\"hooks\":[]}");

        let logical = json!({
            "schema_version": 1,
            "manifest_kind": "logical-runtime-v1",
            "release_version": "0.8.3",
            "runtime_revision": "2",
            "build_identity": digest(b"fixed logical build identity"),
            "artifacts": [{
                "path": "hooks/hooks.json",
                "type": "file",
                "digest_mode": "canonical-json-v1",
                "sha256": digest(b"{\"hooks\":[]}"),
            }],
            "identity_inputs": {
                "algorithm": "sha256-framed-logical-source-and-artifact-projection-v3",
                "inventory_path": "registry/scripts/bundle-spec.json",
                "source_paths": [],
                "artifact_paths": ["hooks/hooks.json"],
            }
        });
        write(
            root.path(),
            LOGICAL_MANIFEST_PATH,
            &serde_json::to_vec(&logical).unwrap(),
        );

        let mut outer = json!({
            "schema_version": 1,
            "manifest_kind": "post-link-runtime-bundle-v1",
            "bundle_id_algorithm": "sha256-canonical-post-link-runtime-bundle-v1",
            "selector_protocol": "epic-harness-materialized-runtime-v1",
            "logical_build_identity": logical["build_identity"],
            "target_triple": TARGET,
            "executable": descriptor(root.path(), "bin/epic-harness", "target-executable-v1"),
            "materialized_files": [
                descriptor(root.path(), "hooks/hooks.json", ""),
                descriptor(root.path(), LOGICAL_MANIFEST_PATH, ""),
            ],
            "bundle_id": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        });
        set_bundle_id(&mut outer);
        let outer_path = root.path().join("outer.json");
        fs::write(&outer_path, serde_json::to_vec(&outer).unwrap()).unwrap();
        Fixture {
            root,
            outer: outer_path,
        }
    }

    fn descriptor(root: &Path, relative: &str, selector: &str) -> Value {
        let path = root.join(relative);
        let bytes = fs::read(&path).unwrap();
        let mode = mode(&path);
        let mut descriptor = json!({
            "path": relative,
            "type": "file",
            "mode": mode,
            "size": bytes.len(),
            "digest_mode": "raw-bytes-v1",
            "sha256": digest(&bytes),
        });
        if !selector.is_empty() {
            descriptor
                .as_object_mut()
                .unwrap()
                .insert("selector".to_owned(), json!(selector));
        }
        descriptor
    }

    fn set_bundle_id(outer: &mut Value) {
        let mut without_id = outer.clone();
        without_id.as_object_mut().unwrap().remove("bundle_id");
        let canonical = super::canonical_json(&without_id).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(OUTER_BUNDLE_ID_DOMAIN);
        hasher.update(canonical);
        outer["bundle_id"] = json!(format!("sha256:{:x}", hasher.finalize()));
    }

    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> String {
        use std::os::unix::fs::MetadataExt;

        format!("{:04o}", fs::metadata(path).unwrap().mode() & 0o777)
    }

    #[cfg(not(unix))]
    fn mode(_path: &Path) -> String {
        "0644".to_owned()
    }

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }
}
