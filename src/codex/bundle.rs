//! The executable-owned Codex bundle projection.
//!
//! Keep the projection here, next to the verification code, rather than
//! deriving it from an installed cache.  An installed cache is the object we
//! are validating and may not be used as authority.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestMode {
    CanonicalPluginJson,
    CanonicalJson,
    NormalizedLfText,
}

impl DigestMode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::CanonicalPluginJson => "canonical-plugin-json-v1",
            Self::CanonicalJson => "canonical-json-v1",
            Self::NormalizedLfText => "normalized-lf-text-v1",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnedArtifact {
    pub(crate) path: &'static str,
    pub(crate) mode: DigestMode,
    source: &'static [u8],
}

impl OwnedArtifact {
    pub(crate) fn expected_bytes(self, cachebuster: Option<&str>) -> Result<Vec<u8>, String> {
        match self.mode {
            DigestMode::CanonicalPluginJson => {
                let mut value = parse_json(self.source, self.path)?;
                let object = value
                    .as_object_mut()
                    .ok_or_else(|| format!("{} must contain a JSON object", self.path))?;
                let source_version = object
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{} must declare a string version", self.path))?;
                let base = plugin_base_version(source_version, self.path)?;
                object.insert(
                    "version".to_owned(),
                    Value::String(match cachebuster {
                        Some(suffix) => format!("{base}+{suffix}"),
                        None => base.to_owned(),
                    }),
                );
                canonical_json(&value)
            }
            DigestMode::CanonicalJson => canonical_json(&parse_json(self.source, self.path)?),
            DigestMode::NormalizedLfText => normalized_lf_text(self.source, self.path),
        }
    }

    pub(crate) fn expected_digest(self) -> Result<String, String> {
        Ok(sha256_prefixed(&self.expected_bytes(None)?))
    }
}

// build.rs expands bundle-spec.json into this projection. It includes the
// checked logical manifest as executable-owned metadata, while the logical
// manifest deliberately excludes its own digest to avoid a self-hash cycle.
include!(concat!(env!("OUT_DIR"), "/bundle_inventory.rs"));

pub(crate) fn find_artifact(path: &str) -> Option<OwnedArtifact> {
    OWNED_ARTIFACTS
        .iter()
        .copied()
        .find(|artifact| artifact.path == path)
}

pub(crate) fn semantic_digest(
    bytes: &[u8],
    mode: DigestMode,
    path: &str,
) -> Result<String, String> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(format!("{path} exceeds {MAX_ARTIFACT_BYTES} byte limit"));
    }
    let canonical = match mode {
        DigestMode::CanonicalPluginJson => canonical_plugin_json(bytes, path)?,
        DigestMode::CanonicalJson => canonical_json(&parse_json(bytes, path)?)?,
        DigestMode::NormalizedLfText => normalized_lf_text(bytes, path)?,
    };
    Ok(sha256_prefixed(&canonical))
}

pub(crate) fn canonical_plugin_json(bytes: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let mut value = parse_json(bytes, path)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{path} must contain a JSON object"))?;
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path} must declare a string version"))?;
    object.insert(
        "version".to_owned(),
        Value::String(plugin_base_version(version, path)?.to_owned()),
    );
    canonical_json(&value)
}

#[cfg(test)]
pub(crate) fn plugin_cachebuster(bytes: &[u8], path: &str) -> Result<Option<String>, String> {
    let value = parse_json(bytes, path)?;
    let version = value
        .as_object()
        .and_then(|object| object.get("version"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path} must declare a string version"))?;
    let _ = plugin_base_version(version, path)?;
    Ok(version.split_once('+').map(|(_, suffix)| suffix.to_owned()))
}

pub(crate) fn plugin_base_version<'a>(version: &'a str, path: &str) -> Result<&'a str, String> {
    let (base, suffix) = match version.split_once('+') {
        Some((base, suffix)) => (base, Some(suffix)),
        None => (version, None),
    };
    let valid_base = base.split('.').count() == 3
        && base
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !valid_base {
        return Err(format!("{path} has an invalid plugin version: {version}"));
    }
    if let Some(suffix) = suffix {
        let valid_suffix = suffix
            .strip_prefix("codex.")
            .filter(|tail| !tail.is_empty())
            .is_some_and(|tail| {
                tail.split('.').all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            });
        if !valid_suffix {
            return Err(format!("{path} has an invalid plugin version: {version}"));
        }
    }
    Ok(base)
}

pub(crate) fn parse_json(bytes: &[u8], path: &str) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("invalid JSON in {path}: {error}"))
}

/// This mirrors `generate-bundle-manifest.js`: every object is recursively
/// sorted before serialisation.  `serde_json::Value`'s map implementation is
/// intentionally not treated as a sorting contract here.
pub(crate) fn canonical_json(value: &Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&sort_json(value))
        .map_err(|error| format!("cannot canonicalize JSON: {error}"))
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sort_json).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), sort_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        primitive => primitive.clone(),
    }
}

pub(crate) fn normalized_lf_text(bytes: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("{path} is not UTF-8: {error}"))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n").into_bytes())
}

pub(crate) fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_prefixed(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn plugin_digest_ignores_codex_cachebuster() {
        let artifact = find_artifact(".codex-plugin/plugin.json").unwrap();
        let original = artifact.expected_bytes(None).unwrap();
        let cached = artifact
            .expected_bytes(Some("codex.20260731-test"))
            .unwrap();
        assert_eq!(
            semantic_digest(&original, artifact.mode, artifact.path).unwrap(),
            semantic_digest(&cached, artifact.mode, artifact.path).unwrap()
        );
        assert_eq!(
            plugin_cachebuster(&cached, artifact.path)
                .unwrap()
                .as_deref(),
            Some("codex.20260731-test")
        );
    }

    #[test]
    fn canonical_json_recursively_sorts_keys() {
        let left = br#"{"z": {"b": 1, "a": 2}, "a": 3}"#;
        let right = br#"{"a":3,"z":{"a":2,"b":1}}"#;
        assert_eq!(
            semantic_digest(left, DigestMode::CanonicalJson, "fixture.json").unwrap(),
            semantic_digest(right, DigestMode::CanonicalJson, "fixture.json").unwrap()
        );
    }
}
