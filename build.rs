use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REVISION_FILE: &str = "runtime-revision.txt";
const BUNDLE_SPEC_FILE: &str = "registry/scripts/bundle-spec.json";
const LOGICAL_MANIFEST_KIND: &str = "logical-runtime-v1";
const OUTER_MANIFEST_KIND: &str = "post-link-runtime-bundle-v1";
const LOGICAL_IDENTITY_ALGORITHM: &str = "sha256-framed-logical-source-and-artifact-projection-v3";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleSpec {
    schema_version: u32,
    logical: LogicalSpec,
    outer: OuterSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalSpec {
    schema_version: u32,
    manifest_kind: String,
    manifest_path: String,
    identity: LogicalIdentitySpec,
    artifacts: Vec<ArtifactSelector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalIdentitySpec {
    algorithm: String,
    domain: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactSelector {
    path: String,
    selector: String,
    digest_mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OuterSpec {
    schema_version: u32,
    manifest_kind: String,
    selector_protocol: String,
    target_executable: TargetExecutableSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetExecutableSpec {
    selector: String,
    #[serde(rename = "type")]
    file_type: String,
    digest_mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    schema_version: u32,
    manifest_kind: String,
    release_version: String,
    runtime_revision: String,
    build_identity: String,
    artifacts: Vec<Artifact>,
    identity_inputs: IdentityInputs,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Artifact {
    path: String,
    #[serde(rename = "type")]
    file_type: String,
    digest_mode: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityInputs {
    algorithm: String,
    inventory_path: String,
    source_paths: Vec<String>,
    artifact_paths: Vec<String>,
}

fn main() {
    let root = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let spec = read_bundle_spec(&root)
        .unwrap_or_else(|error| panic!("cannot read bundle inventory: {error}"));
    let source_paths = source_identity_paths(&root)
        .unwrap_or_else(|error| panic!("cannot collect build identity sources: {error}"));
    let artifact_descriptors = expand_logical_artifacts(&root, &spec)
        .unwrap_or_else(|error| panic!("cannot expand bundle inventory: {error}"));

    println!("cargo:rerun-if-changed=src");
    for path in source_paths
        .iter()
        .map(String::as_str)
        .chain(
            artifact_descriptors
                .iter()
                .map(|artifact| artifact.path.as_str()),
        )
        .chain([spec.logical.manifest_path.as_str()])
    {
        println!("cargo:rerun-if-changed={path}");
    }

    validate_package_closure(&root, &artifact_descriptors, &spec.logical.manifest_path)
        .unwrap_or_else(|error| panic!("runtime package closure is incomplete: {error}"));
    generate_rust_projection(&artifact_descriptors, &spec.logical.manifest_path)
        .unwrap_or_else(|error| panic!("cannot generate Rust bundle projection: {error}"));

    let raw_revision = fs::read_to_string(root.join(REVISION_FILE))
        .unwrap_or_else(|error| panic!("cannot read {REVISION_FILE}: {error}"));
    let revision = raw_revision.trim();
    assert!(
        !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit()),
        "{REVISION_FILE} must contain one positive integer"
    );
    let parsed_revision = revision
        .parse::<u64>()
        .unwrap_or_else(|error| panic!("invalid {REVISION_FILE}: {error}"));
    assert!(
        parsed_revision > 0,
        "{REVISION_FILE} must be greater than zero"
    );

    let artifacts = expected_artifacts(&root, &artifact_descriptors)
        .unwrap_or_else(|error| panic!("cannot project bundle artifacts: {error}"));
    let build_identity = build_identity(&root, &spec, &source_paths, &artifacts)
        .unwrap_or_else(|error| panic!("cannot compute build identity: {error}"));
    validate_bundle_manifest(
        &root,
        &spec,
        revision,
        &source_paths,
        &artifacts,
        &build_identity,
    )
    .unwrap_or_else(|error| {
        panic!(
            "bundle manifest is stale: {error}. Run `node registry/scripts/generate-bundle-manifest.js --write`"
        )
    });

    println!("cargo:rustc-env=EPIC_HARNESS_RUNTIME_REVISION={parsed_revision}");
    println!("cargo:rustc-env=EPIC_HARNESS_BUILD_IDENTITY={build_identity}");
}

fn read_bundle_spec(root: &Path) -> Result<BundleSpec, String> {
    let path = root.join(BUNDLE_SPEC_FILE);
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let spec = serde_json::from_slice::<BundleSpec>(&bytes)
        .map_err(|error| format!("invalid {BUNDLE_SPEC_FILE}: {error}"))?;
    validate_bundle_spec(&spec)?;
    Ok(spec)
}

fn validate_bundle_spec(spec: &BundleSpec) -> Result<(), String> {
    if spec.schema_version != 1 {
        return Err(format!(
            "bundle spec schema version is {}, expected 1",
            spec.schema_version
        ));
    }
    if spec.logical.schema_version != 1 || spec.logical.manifest_kind != LOGICAL_MANIFEST_KIND {
        return Err("logical manifest schema is unsupported".to_owned());
    }
    validate_portable_path(&spec.logical.manifest_path, "logical manifest path")?;
    if spec.logical.identity.algorithm != LOGICAL_IDENTITY_ALGORITHM {
        return Err("logical identity algorithm is unsupported".to_owned());
    }
    if spec.logical.identity.domain.is_empty() {
        return Err("logical identity domain is empty".to_owned());
    }
    if spec.logical.artifacts.is_empty() {
        return Err("logical artifact inventory is empty".to_owned());
    }
    for artifact in &spec.logical.artifacts {
        validate_portable_path(&artifact.path, "logical artifact path")?;
        if artifact.path == spec.logical.manifest_path {
            return Err("logical manifest cannot digest itself".to_owned());
        }
        if !matches!(artifact.selector.as_str(), "file-v1" | "recursive-files-v1") {
            return Err(format!(
                "logical artifact {} has unsupported selector {}",
                artifact.path, artifact.selector
            ));
        }
        if !matches!(
            artifact.digest_mode.as_str(),
            "canonical-plugin-json-v1" | "canonical-json-v1" | "normalized-lf-text-v1"
        ) {
            return Err(format!(
                "logical artifact {} has unsupported digest mode {}",
                artifact.path, artifact.digest_mode
            ));
        }
    }
    if spec.outer.schema_version != 1 || spec.outer.manifest_kind != OUTER_MANIFEST_KIND {
        return Err("outer manifest schema is unsupported".to_owned());
    }
    if spec.outer.selector_protocol != "epic-harness-materialized-runtime-v1" {
        return Err("outer selector protocol is unsupported".to_owned());
    }
    if spec.outer.target_executable.selector != "target-executable-v1"
        || spec.outer.target_executable.file_type != "file"
        || spec.outer.target_executable.digest_mode != "raw-bytes-v1"
    {
        return Err("outer target executable selector is unsupported".to_owned());
    }
    Ok(())
}

fn validate_portable_path(path: &str, label: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(format!("{label} must be a portable relative path: {path}"));
    }
    for component in path.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(format!(
                "{label} has traversal or unsupported components: {path}"
            ));
        }
    }
    Ok(())
}

fn source_identity_paths(root: &Path) -> Result<Vec<String>, String> {
    let mut paths = vec![
        "Cargo.lock".to_owned(),
        "Cargo.toml".to_owned(),
        "build.rs".to_owned(),
        BUNDLE_SPEC_FILE.to_owned(),
    ];
    collect_rust_sources(root, &root.join("src"), &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_rust_sources(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<String>,
) -> Result<(), String> {
    assert_real_directory(directory, "Rust source directory")?;
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read Rust source directory entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Rust source tree contains a symlink: {}",
                entry.path().display()
            ));
        }
        if metadata.is_dir() {
            collect_rust_sources(root, &entry.path(), paths)?;
        } else if metadata.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
        {
            paths.push(relative_path(root, &entry.path())?);
        }
    }
    Ok(())
}

fn expand_logical_artifacts(root: &Path, spec: &BundleSpec) -> Result<Vec<Artifact>, String> {
    let mut paths = BTreeSet::new();
    let mut artifacts = Vec::new();
    for selector in &spec.logical.artifacts {
        let absolute = root.join(&selector.path);
        let selected = match selector.selector.as_str() {
            "file-v1" => {
                assert_regular_file(&absolute, &format!("logical artifact {}", selector.path))?;
                vec![selector.path.clone()]
            }
            "recursive-files-v1" => collect_regular_files(root, &absolute, &selector.path)?,
            _ => unreachable!("bundle spec validation accepts only known selectors"),
        };
        if selected.is_empty() {
            return Err(format!(
                "logical artifact {} selected no files",
                selector.path
            ));
        }
        for path in selected {
            if !paths.insert(path.clone()) {
                return Err(format!("duplicate logical artifact path: {path}"));
            }
            artifacts.push(Artifact {
                path,
                file_type: "file".to_owned(),
                digest_mode: selector.digest_mode.clone(),
                sha256: String::new(),
            });
        }
    }
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    label: &str,
) -> Result<Vec<String>, String> {
    assert_real_directory(directory, label)?;
    let mut paths = Vec::new();
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {label} directory entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{label} contains a symlink: {}",
                entry.path().display()
            ));
        }
        if metadata.is_dir() {
            paths.extend(collect_regular_files(root, &entry.path(), label)?);
        } else if metadata.is_file() {
            paths.push(relative_path(root, &entry.path())?);
        } else {
            return Err(format!(
                "{label} contains an unsupported entry: {}",
                entry.path().display()
            ));
        }
    }
    paths.sort();
    Ok(paths)
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .map_err(|error| format!("cannot relativize {}: {error}", path.display()))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn assert_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{label} must be a regular non-symlink file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn assert_real_directory(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{label} must be a real non-symlink directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn expected_artifacts(root: &Path, descriptors: &[Artifact]) -> Result<Vec<Artifact>, String> {
    descriptors
        .iter()
        .map(|descriptor| {
            Ok(Artifact {
                path: descriptor.path.clone(),
                file_type: descriptor.file_type.clone(),
                digest_mode: descriptor.digest_mode.clone(),
                sha256: artifact_digest(root, &descriptor.path, &descriptor.digest_mode)?,
            })
        })
        .collect()
}

fn artifact_digest(root: &Path, path: &str, digest_mode: &str) -> Result<String, String> {
    let bytes =
        fs::read(root.join(path)).map_err(|error| format!("cannot read {path}: {error}"))?;
    let canonical = match digest_mode {
        "canonical-plugin-json-v1" => canonical_plugin_json(&bytes, path)?,
        "canonical-json-v1" => canonical_json(&bytes, path)?,
        "normalized-lf-text-v1" => normalized_lf_text(&bytes, path)?,
        _ => return Err(format!("unsupported artifact digest mode {digest_mode}")),
    };
    Ok(sha256(&canonical))
}

fn canonical_json(bytes: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|error| format!("invalid JSON in {path}: {error}"))?;
    canonical_json_value(&value)
}

fn canonical_json_value(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    fn sort(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(sort).collect())
            }
            serde_json::Value::Object(values) => serde_json::Value::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), sort(value)))
                    .collect(),
            ),
            primitive => primitive.clone(),
        }
    }
    serde_json::to_vec(&sort(value)).map_err(|error| format!("cannot canonicalize JSON: {error}"))
}

fn canonical_plugin_json(bytes: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let mut value = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|error| format!("invalid JSON in {path}: {error}"))?;
    let plugin = value
        .as_object_mut()
        .ok_or_else(|| format!("{path} must contain a JSON object"))?;
    let version = plugin
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{path} must declare a string version"))?;
    plugin.insert(
        "version".to_owned(),
        serde_json::Value::String(plugin_base_version(version, path)?),
    );
    canonical_json_value(&value)
}

fn plugin_base_version(version: &str, path: &str) -> Result<String, String> {
    let (base, suffix) = match version.split_once('+') {
        Some((base, suffix)) => (base, Some(suffix)),
        None => (version, None),
    };
    if !base
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        || base.split('.').count() != 3
    {
        return Err(format!("{path} has an invalid plugin version: {version}"));
    }
    if let Some(suffix) = suffix {
        let cachebuster = suffix
            .strip_prefix("codex.")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{path} has an invalid plugin version: {version}"))?;
        if !cachebuster.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(format!("{path} has an invalid plugin version: {version}"));
        }
    }
    Ok(base.to_owned())
}

fn normalized_lf_text(bytes: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("{path} is not UTF-8: {error}"))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n").into_bytes())
}

fn build_identity(
    root: &Path,
    spec: &BundleSpec,
    source_paths: &[String],
    artifacts: &[Artifact],
) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(spec.logical.identity.domain.as_bytes());
    for path in source_paths {
        frame(&mut digest, b"source");
        frame(&mut digest, path.as_bytes());
        let bytes =
            fs::read(root.join(path)).map_err(|error| format!("cannot read {path}: {error}"))?;
        let normalized = normalize_source_identity(&bytes);
        frame(&mut digest, &normalized);
    }
    for artifact in artifacts {
        frame(&mut digest, b"artifact");
        frame(&mut digest, artifact.path.as_bytes());
        frame(&mut digest, artifact.file_type.as_bytes());
        frame(&mut digest, artifact.digest_mode.as_bytes());
        frame(&mut digest, artifact.sha256.as_bytes());
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn normalize_source_identity(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            normalized.push(b'\n');
            index += 1;
            if index < bytes.len() && bytes[index] == b'\n' {
                index += 1;
            }
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn validate_package_closure(
    root: &Path,
    artifacts: &[Artifact],
    manifest_path: &str,
) -> Result<(), String> {
    let package = fs::read(root.join("package.json"))
        .map_err(|error| format!("cannot read package.json: {error}"))?;
    let value = serde_json::from_slice::<serde_json::Value>(&package)
        .map_err(|error| format!("invalid package.json: {error}"))?;
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "package.json files must be an array".to_owned())?
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "package.json files must contain only strings".to_owned())?;
    for path in artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .chain([manifest_path])
        .filter(|path| *path != "package.json")
    {
        if !files.iter().any(|entry| package_declares_path(entry, path)) {
            return Err(format!(
                "package.json files does not ship required runtime path {path}"
            ));
        }
    }
    Ok(())
}

fn package_declares_path(entry: &str, path: &str) -> bool {
    let declared = entry.trim_end_matches('/');
    path == declared
        || path
            .strip_prefix(declared)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn generate_rust_projection(artifacts: &[Artifact], manifest_path: &str) -> Result<(), String> {
    let mut projection = artifacts.to_vec();
    projection.push(Artifact {
        path: manifest_path.to_owned(),
        file_type: "file".to_owned(),
        digest_mode: "canonical-json-v1".to_owned(),
        sha256: String::new(),
    });
    projection.sort_by(|left, right| left.path.cmp(&right.path));
    if projection
        .windows(2)
        .any(|pair| pair[0].path == pair[1].path)
    {
        return Err("generated Rust projection contains a duplicate path".to_owned());
    }

    let mut generated = String::from(
        "// Generated by build.rs from registry/scripts/bundle-spec.json. Do not edit.\n\
         pub(crate) const OWNED_ARTIFACTS: &[OwnedArtifact] = &[\n",
    );
    for artifact in &projection {
        let mode = match artifact.digest_mode.as_str() {
            "canonical-plugin-json-v1" => "DigestMode::CanonicalPluginJson",
            "canonical-json-v1" => "DigestMode::CanonicalJson",
            "normalized-lf-text-v1" => "DigestMode::NormalizedLfText",
            _ => {
                return Err(format!(
                    "cannot project unsupported digest mode {}",
                    artifact.digest_mode
                ));
            }
        };
        let path = serde_json::to_string(&artifact.path)
            .map_err(|error| format!("cannot quote generated artifact path: {error}"))?;
        generated.push_str(&format!(
            "    OwnedArtifact {{ path: {path}, mode: {mode}, source: include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/\", {path})) }},\n"
        ));
    }
    generated.push_str("];\n");
    let out_dir =
        std::env::var_os("OUT_DIR").ok_or_else(|| "Cargo did not provide OUT_DIR".to_owned())?;
    fs::write(Path::new(&out_dir).join("bundle_inventory.rs"), generated)
        .map_err(|error| format!("cannot write generated bundle projection: {error}"))
}

fn validate_bundle_manifest(
    root: &Path,
    spec: &BundleSpec,
    runtime_revision: &str,
    source_paths: &[String],
    artifacts: &[Artifact],
    build_identity: &str,
) -> Result<(), String> {
    let manifest_path = root.join(&spec.logical.manifest_path);
    let bytes = fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let manifest = serde_json::from_slice::<BundleManifest>(&bytes)
        .map_err(|error| format!("invalid {}: {error}", manifest_path.display()))?;
    if manifest.schema_version != spec.logical.schema_version {
        return Err(format!(
            "schema version is {}, expected {}",
            manifest.schema_version, spec.logical.schema_version
        ));
    }
    if manifest.manifest_kind != spec.logical.manifest_kind {
        return Err("manifest kind differs".to_owned());
    }
    let release_version = cargo_release_version(root)?;
    if manifest.release_version != release_version {
        return Err(format!(
            "release version is {}, expected {release_version}",
            manifest.release_version
        ));
    }
    if manifest.runtime_revision != runtime_revision {
        return Err(format!(
            "runtime revision is {}, expected {runtime_revision}",
            manifest.runtime_revision
        ));
    }
    if manifest.artifacts != artifacts {
        return Err("artifact projection differs".to_owned());
    }
    if manifest.identity_inputs.algorithm != spec.logical.identity.algorithm {
        return Err("build identity algorithm differs".to_owned());
    }
    if manifest.identity_inputs.inventory_path != BUNDLE_SPEC_FILE {
        return Err("build identity inventory path differs".to_owned());
    }
    if manifest.identity_inputs.source_paths != source_paths {
        return Err("source identity path projection differs".to_owned());
    }
    let artifact_paths = artifacts
        .iter()
        .map(|artifact| artifact.path.clone())
        .collect::<Vec<_>>();
    if manifest.identity_inputs.artifact_paths != artifact_paths {
        return Err("artifact identity path projection differs".to_owned());
    }
    if manifest.build_identity != build_identity {
        return Err("build identity differs".to_owned());
    }
    Ok(())
}

fn cargo_release_version(root: &Path) -> Result<String, String> {
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|error| format!("cannot read Cargo.toml: {error}"))?;
    let value = toml::from_str::<toml::Table>(&cargo_toml)
        .map_err(|error| format!("cannot parse Cargo.toml: {error}"))?;
    value
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Cargo.toml package.version is missing".to_owned())
}
