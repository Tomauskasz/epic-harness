//! Durable filesystem implementation of the runtime-bundle platform seam.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    Fence, Generation, Journal, JournalPhase, Platform, SelectorSnapshot, TransactionError,
};

const OWNER_FILE: &str = ".epic-runtime-bundle-owner";
const OWNER_BYTES: &[u8] = b"epic-harness-runtime-bundle-v1\n";
const REPAIR_LOCK_FILE: &str = "repair.lock";
const FENCE_FILE: &str = "fence";
const SELECTOR_FILE: &str = "active.json";
const JOURNAL_FILE: &str = "journal.json";
const GENERATIONS_DIR: &str = "generations";
const GENERATION_METADATA_FILE: &str = "generation.json";
const GENERATION_EXECUTABLE_FILE: &str = "executable";
const GENERATION_PROJECTION_FILE: &str = "projection.bin";
const GENERATION_FILES_DIR: &str = "files";
const MAX_SELECTOR_BYTES: u64 = 4 * 1024;
const MAX_JOURNAL_BYTES: u64 = 128 * 1024;
const MAX_GENERATION_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_GENERATION_FILES: usize = 4_096;
static TEMPORARY_NONCE: AtomicU64 = AtomicU64::new(0);

/// One persistent runtime-bundle layout.  The owned marker is deliberately
/// separate from the mutable selector and journal, so an unrelated directory
/// can never be adopted by a repair operation.
pub struct FilesystemPlatform {
    root: PathBuf,
    held: Option<HeldLock>,
    executable_probe: Arc<dyn ExecutableProbe>,
}

/// The only supported resolution of a selected immutable generation.  Callers
/// never construct an executable path from an id themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedGeneration {
    pub bundle_id: String,
    pub generation_dir: PathBuf,
    pub executable: PathBuf,
    pub files_dir: PathBuf,
}

#[derive(Debug)]
struct HeldLock {
    _file: File,
    fence: Fence,
}

/// The executable proof required before selector publication.  An
/// implementation must run the exact supplied generation executable with
/// `version --json`, bound its output/time, and reject a non-zero or invalid
/// response.  The filesystem layer deliberately cannot replace this host
/// policy with byte comparison.
pub trait ExecutableProbe: Send + Sync {
    fn verify_version_json(&self, executable: &Path) -> Result<(), String>;
}

struct MissingExecutableProbe;

impl ExecutableProbe for MissingExecutableProbe {
    fn verify_version_json(&self, executable: &Path) -> Result<(), String> {
        Err(format!(
            "no executable probe was configured for {}; construct FilesystemPlatform with open_with_probe",
            executable.display()
        ))
    }
}

impl FilesystemPlatform {
    /// Open or initialize a single Epic-owned layout.  Initialization refuses
    /// a nonempty unowned directory rather than treating it as recoverable
    /// runtime state.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, TransactionError> {
        Self::open_with_probe(root, MissingExecutableProbe)
    }

    /// Construct a platform whose staged executable is proven through the
    /// supplied bounded `version --json` host probe.
    pub fn open_with_probe(
        root: impl Into<PathBuf>,
        executable_probe: impl ExecutableProbe + 'static,
    ) -> Result<Self, TransactionError> {
        let platform = Self {
            root: root.into(),
            held: None,
            executable_probe: Arc::new(executable_probe),
        };
        platform.ensure_owned_layout()?;
        Ok(platform)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn owner_path(&self) -> PathBuf {
        self.root.join(OWNER_FILE)
    }

    fn repair_lock_path(&self) -> PathBuf {
        self.root.join(REPAIR_LOCK_FILE)
    }

    fn fence_path(&self) -> PathBuf {
        self.root.join(FENCE_FILE)
    }

    fn selector_path(&self) -> PathBuf {
        self.root.join(SELECTOR_FILE)
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join(JOURNAL_FILE)
    }

    fn generations_path(&self) -> PathBuf {
        self.root.join(GENERATIONS_DIR)
    }

    /// Resolve one valid bundle id into the fixed immutable layout.  This is
    /// intentionally the sole id-to-path authority for Rust callers.
    pub fn resolve_pinned_generation(
        &self,
        bundle_id: &str,
    ) -> Result<PinnedGeneration, TransactionError> {
        self.pinned_generation(bundle_id, TransactionError::Verification)
    }

    /// Resolve the current `active.json` only after its referenced immutable
    /// payload has been fully validated.  This is the read-side counterpart to
    /// selector publication; a syntactically valid selector never authorizes a
    /// missing or corrupted generation.
    pub fn active_pinned_generation(&self) -> Result<Option<PinnedGeneration>, TransactionError> {
        let selector = self.selector_snapshot()?;
        let Some(bundle_id) = selector.generation_id else {
            return Ok(None);
        };
        self.read_generation(&bundle_id, TransactionError::Verification)?;
        self.resolve_pinned_generation(&bundle_id).map(Some)
    }

    fn pinned_generation(
        &self,
        bundle_id: &str,
        error: fn(String) -> TransactionError,
    ) -> Result<PinnedGeneration, TransactionError> {
        let hex = bundle_id_hex(bundle_id, error)?;
        let generation_dir = self.generations_path().join(hex);
        Ok(PinnedGeneration {
            bundle_id: bundle_id.to_owned(),
            executable: generation_dir.join(GENERATION_EXECUTABLE_FILE),
            files_dir: generation_dir.join(GENERATION_FILES_DIR),
            generation_dir,
        })
    }

    fn ensure_owned_layout(&self) -> Result<(), TransactionError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if is_real_directory(&metadata) => {}
            Ok(_) => {
                return Err(TransactionError::Stage(format!(
                    "runtime-bundle root is not a regular non-symlink directory: {}",
                    self.root.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.root).map_err(|error| {
                    TransactionError::Stage(format!(
                        "cannot create runtime-bundle root {}: {error}",
                        self.root.display()
                    ))
                })?;
                ensure_real_directory(&self.root, "runtime-bundle root", TransactionError::Stage)?;
            }
            Err(error) => {
                return Err(TransactionError::Stage(format!(
                    "cannot inspect runtime-bundle root {}: {error}",
                    self.root.display()
                )));
            }
        }

        match fs::symlink_metadata(self.owner_path()) {
            Ok(metadata) => {
                ensure_real_file_metadata(
                    &metadata,
                    "runtime-bundle ownership marker",
                    &self.owner_path(),
                    TransactionError::Stage,
                )?;
                let bytes = fs::read(self.owner_path()).map_err(|error| {
                    TransactionError::Stage(format!(
                        "cannot read runtime-bundle ownership marker: {error}"
                    ))
                })?;
                if bytes != OWNER_BYTES {
                    return Err(TransactionError::Stage(format!(
                        "runtime-bundle root has an unrecognized Epic ownership marker: {}",
                        self.root.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut entries = fs::read_dir(&self.root).map_err(|error| {
                    TransactionError::Stage(format!("cannot inspect runtime-bundle root: {error}"))
                })?;
                if entries
                    .next()
                    .transpose()
                    .map_err(|error| {
                        TransactionError::Stage(format!(
                            "cannot inspect runtime-bundle root entry: {error}"
                        ))
                    })?
                    .is_some()
                {
                    return Err(TransactionError::Stage(format!(
                        "refusing to adopt nonempty unowned runtime-bundle root: {}",
                        self.root.display()
                    )));
                }
                write_new_file(&self.owner_path(), OWNER_BYTES, TransactionError::Stage)?;
                sync_directory(&self.root, TransactionError::Stage)?;
            }
            Err(error) => {
                return Err(TransactionError::Stage(format!(
                    "cannot inspect runtime-bundle ownership marker: {error}"
                )));
            }
        }

        ensure_or_create_directory(&self.generations_path(), TransactionError::Stage)?;
        ensure_or_create_regular_file(&self.repair_lock_path(), TransactionError::Stage)?;
        Ok(())
    }

    fn require_fence(&self, fence: Fence) -> Result<(), TransactionError> {
        (self.held.as_ref().map(|held| held.fence) == Some(fence))
            .then_some(())
            .ok_or(TransactionError::FenceMismatch)
    }

    fn next_fence(&self) -> Result<Fence, TransactionError> {
        let path = self.fence_path();
        let current = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure_real_file_metadata(
                    &metadata,
                    "runtime-bundle fence",
                    &path,
                    TransactionError::Stage,
                )?;
                let bytes = read_bounded_file(
                    &path,
                    "runtime-bundle fence",
                    MAX_SELECTOR_BYTES,
                    TransactionError::Stage,
                )?;
                parse_counter(&bytes)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(TransactionError::Stage(format!(
                    "cannot inspect runtime-bundle fence: {error}"
                )));
            }
        };
        let next = current.checked_add(1).ok_or_else(|| {
            TransactionError::Stage("runtime-bundle fence counter is exhausted".to_owned())
        })?;
        durable_replace(
            &path,
            format!("{next}\n").as_bytes(),
            TransactionError::Stage,
        )?;
        Ok(Fence(next))
    }

    fn read_generation(
        &self,
        id: &str,
        error: fn(String) -> TransactionError,
    ) -> Result<Generation, TransactionError> {
        let pinned = self.pinned_generation(id, error)?;
        ensure_real_directory(&pinned.generation_dir, "generation directory", error)?;
        let descriptor_path = pinned.generation_dir.join(GENERATION_METADATA_FILE);
        let bytes = read_bounded_file(
            &descriptor_path,
            "generation metadata",
            MAX_GENERATION_METADATA_BYTES,
            error,
        )?;
        let descriptor: GenerationDescriptor = serde_json::from_slice(&bytes).map_err(|cause| {
            error(format!(
                "invalid generation metadata {}: {cause}",
                descriptor_path.display()
            ))
        })?;
        if descriptor.id != id || descriptor.files.len() > MAX_GENERATION_FILES {
            return Err(error(format!(
                "generation metadata does not match requested generation {id}"
            )));
        }
        validate_generation_descriptor(&descriptor, error)?;
        assert_generation_entries_exact(&pinned.generation_dir, &descriptor, error)?;

        let executable = read_descriptor_file(
            &pinned.executable,
            &descriptor.executable,
            "staged executable",
            error,
        )?;
        let projection = read_descriptor_file(
            &pinned.generation_dir.join(GENERATION_PROJECTION_FILE),
            &descriptor.projection,
            "staged host projection",
            error,
        )?;
        let mut files = BTreeMap::new();
        for file in &descriptor.files {
            let path = relative_child(&pinned.files_dir, &file.path, "generation file", error)?;
            let bytes = read_descriptor_file(&path, file, "staged generation file", error)?;
            if files.insert(file.path.clone(), bytes).is_some() {
                return Err(error(format!(
                    "generation metadata duplicates file {}",
                    file.path
                )));
            }
        }
        Ok(Generation {
            id: descriptor.id.clone(),
            executable,
            files,
            projection: super::HostProjection {
                generation_id: descriptor.projection_generation_id,
                bytes: projection,
            },
        })
    }

    fn generation_matches(
        &self,
        generation: &Generation,
        error: fn(String) -> TransactionError,
    ) -> Result<(), TransactionError> {
        let stored = self.read_generation(&generation.id, error)?;
        if &stored != generation {
            return Err(error(format!(
                "existing immutable generation {} differs from the staged candidate",
                generation.id
            )));
        }
        Ok(())
    }

    fn write_generation(&self, generation: &Generation) -> Result<(), TransactionError> {
        validate_generation(generation, TransactionError::Stage)?;
        let generations = self.generations_path();
        ensure_real_directory(
            &generations,
            "generations directory",
            TransactionError::Stage,
        )?;
        let destination = self
            .pinned_generation(&generation.id, TransactionError::Stage)?
            .generation_dir;
        match fs::symlink_metadata(&destination) {
            Ok(_) => return self.generation_matches(generation, TransactionError::Stage),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(TransactionError::Stage(format!(
                    "cannot inspect immutable generation destination: {error}"
                )));
            }
        }

        let staging = fresh_directory(&generations, "stage", TransactionError::Stage)?;
        let staged = self.write_generation_contents(&staging, generation);
        if let Err(error) = staged {
            return Err(cleanup_directory_error(
                &staging,
                error,
                TransactionError::Stage,
            ));
        }
        sync_directory(&staging, TransactionError::Stage)?;
        match fs::rename(&staging, &destination) {
            Ok(()) => {
                sync_directory(&generations, TransactionError::Stage)?;
                self.generation_matches(generation, TransactionError::Stage)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let cleanup = fs::remove_dir_all(&staging);
                match self.generation_matches(generation, TransactionError::Stage) {
                    Ok(()) => cleanup.map_err(|cleanup| {
                        TransactionError::Stage(format!(
                            "immutable generation already existed, but staging cleanup failed: {cleanup}"
                        ))
                    }),
                    Err(primary) => Err(cleanup_error(primary, cleanup, TransactionError::Stage)),
                }
            }
            Err(error) => Err(cleanup_directory_error(
                &staging,
                TransactionError::Stage(format!(
                    "cannot publish fresh immutable generation {}: {error}",
                    generation.id
                )),
                TransactionError::Stage,
            )),
        }
    }

    fn write_generation_contents(
        &self,
        directory: &Path,
        generation: &Generation,
    ) -> Result<(), TransactionError> {
        let files_root = directory.join(GENERATION_FILES_DIR);
        fs::create_dir(&files_root).map_err(|error| {
            TransactionError::Stage(format!(
                "cannot create staged generation file root: {error}"
            ))
        })?;
        write_new_file(
            &directory.join(GENERATION_EXECUTABLE_FILE),
            &generation.executable,
            TransactionError::Stage,
        )?;
        make_executable(
            &directory.join(GENERATION_EXECUTABLE_FILE),
            TransactionError::Stage,
        )?;
        write_new_file(
            &directory.join(GENERATION_PROJECTION_FILE),
            &generation.projection.bytes,
            TransactionError::Stage,
        )?;
        for (path, bytes) in &generation.files {
            let path = relative_child(
                &files_root,
                path,
                "generation file",
                TransactionError::Stage,
            )?;
            let parent = path.parent().ok_or_else(|| {
                TransactionError::Stage("generation file has no parent directory".to_owned())
            })?;
            create_directory_chain(&files_root, parent, TransactionError::Stage)?;
            write_new_file(&path, bytes, TransactionError::Stage)?;
        }
        let descriptor = GenerationDescriptor::from_generation(generation);
        let descriptor = serde_json::to_vec(&descriptor).map_err(|error| {
            TransactionError::Stage(format!("cannot serialize generation metadata: {error}"))
        })?;
        write_new_file(
            &directory.join(GENERATION_METADATA_FILE),
            &descriptor,
            TransactionError::Stage,
        )?;
        sync_directory(&files_root, TransactionError::Stage)?;
        Ok(())
    }

    fn write_selector(
        &self,
        selector: &SelectorSnapshot,
        fence: Fence,
        error: fn(String) -> TransactionError,
    ) -> Result<(), TransactionError> {
        self.require_fence(fence)?;
        validate_selector(selector, error)?;
        if let Some(id) = &selector.generation_id {
            self.generation_matches_by_id(id, error)?;
            // This is the single active-selector commit point.  No caller
            // writes `active.json` by another path.
            durable_replace(&self.selector_path(), &selector.bytes, error)
        } else {
            let path = self.selector_path();
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    ensure_real_file_metadata(&metadata, "active selector", &path, error)?;
                    fs::remove_file(&path).map_err(|cause| {
                        error(format!("cannot remove absent active selector: {cause}"))
                    })?;
                    sync_directory(&self.root, error)
                }
                Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(cause) => Err(error(format!("cannot inspect active selector: {cause}"))),
            }
        }
    }

    fn generation_matches_by_id(
        &self,
        id: &str,
        error: fn(String) -> TransactionError,
    ) -> Result<(), TransactionError> {
        self.read_generation(id, error).map(|_| ())
    }
}

impl Platform for FilesystemPlatform {
    fn acquire_lock(&mut self) -> Result<Fence, TransactionError> {
        self.ensure_owned_layout()?;
        if self.held.is_some() {
            return Err(TransactionError::LockContended);
        }
        let lock_path = self.repair_lock_path();
        ensure_regular_file(
            &lock_path,
            "runtime-bundle repair lock",
            TransactionError::Stage,
        )?;
        let Some(file) =
            crate::orchestrate::state::try_acquire_lock(&lock_path).map_err(|error| {
                TransactionError::Stage(format!(
                    "cannot acquire runtime-bundle repair lock: {error}"
                ))
            })?
        else {
            return Err(TransactionError::LockContended);
        };
        let fence = match self.next_fence() {
            Ok(fence) => fence,
            Err(error) => return Err(error),
        };
        self.held = Some(HeldLock { _file: file, fence });
        Ok(fence)
    }

    fn release_lock(&mut self, fence: Fence) -> Result<(), TransactionError> {
        self.require_fence(fence)?;
        self.held.take();
        Ok(())
    }

    fn selector_snapshot(&self) -> Result<SelectorSnapshot, TransactionError> {
        let path = self.selector_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure_real_file_metadata(
                    &metadata,
                    "active selector",
                    &path,
                    TransactionError::Verification,
                )?;
                let bytes = read_bounded_file(
                    &path,
                    "active selector",
                    MAX_SELECTOR_BYTES,
                    TransactionError::Verification,
                )?;
                selector_snapshot_from_bytes(&bytes, TransactionError::Verification)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(SelectorSnapshot::new(None, Vec::new()))
            }
            Err(error) => Err(TransactionError::Verification(format!(
                "cannot inspect active selector: {error}"
            ))),
        }
    }

    fn stage_generation(
        &mut self,
        generation: &Generation,
        fence: Fence,
    ) -> Result<(), TransactionError> {
        self.require_fence(fence)?;
        self.ensure_owned_layout()?;
        self.write_generation(generation)
    }

    fn verify_staged_executable(&self, generation: &Generation) -> Result<(), TransactionError> {
        self.generation_matches(generation, TransactionError::Verification)?;
        let pinned = self.pinned_generation(&generation.id, TransactionError::Verification)?;
        self.executable_probe
            .verify_version_json(&pinned.executable)
            .map_err(|error| {
                TransactionError::Verification(format!(
                    "staged executable version --json probe failed for {}: {error}",
                    pinned.executable.display()
                ))
            })
    }

    fn selector_for(&self, generation: &Generation) -> SelectorSnapshot {
        let bytes = canonical_selector_bytes(&generation.id, TransactionError::Stage)
            .expect("stage_generation validates a bundle-id before selector construction");
        SelectorSnapshot::new(Some(generation.id.clone()), bytes)
    }

    fn persist_journal(&mut self, journal: &Journal, fence: Fence) -> Result<(), TransactionError> {
        self.require_fence(fence)?;
        if journal.fence != fence {
            return Err(TransactionError::FenceMismatch);
        }
        self.generation_matches(&journal.generation, TransactionError::Journal)?;
        let disk = DiskJournal::from_journal(journal);
        let bytes = serde_json::to_vec(&disk).map_err(|error| {
            TransactionError::Journal(format!("cannot serialize runtime journal: {error}"))
        })?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(TransactionError::Journal(format!(
                "runtime journal exceeds {MAX_JOURNAL_BYTES} byte limit"
            )));
        }
        durable_replace(&self.journal_path(), &bytes, TransactionError::Journal)
    }

    fn journal(&self) -> Result<Option<Journal>, TransactionError> {
        let path = self.journal_path();
        let bytes = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure_real_file_metadata(
                    &metadata,
                    "runtime journal",
                    &path,
                    TransactionError::Journal,
                )?;
                read_bounded_file(
                    &path,
                    "runtime journal",
                    MAX_JOURNAL_BYTES,
                    TransactionError::Journal,
                )?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(TransactionError::Journal(format!(
                    "cannot inspect runtime journal: {error}"
                )));
            }
        };
        let disk: DiskJournal = serde_json::from_slice(&bytes).map_err(|error| {
            TransactionError::Journal(format!("invalid runtime journal: {error}"))
        })?;
        disk.into_journal(self)
            .map(Some)
            .map_err(|error| match error {
                TransactionError::Journal(_) => error,
                other => TransactionError::Journal(other.to_string()),
            })
    }

    fn clear_journal(&mut self, fence: Fence) -> Result<(), TransactionError> {
        self.require_fence(fence)?;
        let path = self.journal_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure_real_file_metadata(
                    &metadata,
                    "runtime journal",
                    &path,
                    TransactionError::Journal,
                )?;
                fs::remove_file(&path).map_err(|error| {
                    TransactionError::Journal(format!("cannot clear runtime journal: {error}"))
                })?;
                sync_directory(&self.root, TransactionError::Journal)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(TransactionError::Journal(format!(
                "cannot inspect runtime journal before clearing: {error}"
            ))),
        }
    }

    fn publish_selector(
        &mut self,
        selector: &SelectorSnapshot,
        fence: Fence,
    ) -> Result<(), TransactionError> {
        self.write_selector(selector, fence, TransactionError::Publish)
    }

    fn rollback_selector(
        &mut self,
        selector: &SelectorSnapshot,
        fence: Fence,
    ) -> Result<(), TransactionError> {
        self.write_selector(selector, fence, TransactionError::Rollback)
    }

    fn verify_active(
        &self,
        generation: &Generation,
        selector: &SelectorSnapshot,
    ) -> Result<(), TransactionError> {
        if self.selector_snapshot()? != *selector {
            return Err(TransactionError::Verification(
                "active selector bytes differ from the committed selector snapshot".to_owned(),
            ));
        }
        self.generation_matches(generation, TransactionError::Verification)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationDescriptor {
    id: String,
    executable: GenerationFile,
    projection_generation_id: String,
    projection: GenerationFile,
    files: Vec<GenerationFile>,
}

impl GenerationDescriptor {
    fn from_generation(generation: &Generation) -> Self {
        Self {
            id: generation.id.clone(),
            executable: GenerationFile::for_payload(
                GENERATION_EXECUTABLE_FILE,
                &generation.executable,
            ),
            projection_generation_id: generation.projection.generation_id.clone(),
            projection: GenerationFile::for_payload(
                GENERATION_PROJECTION_FILE,
                &generation.projection.bytes,
            ),
            files: generation
                .files
                .iter()
                .map(|(path, bytes)| GenerationFile::for_payload(path, bytes))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationFile {
    path: String,
    size: u64,
    sha256: String,
}

impl GenerationFile {
    fn for_payload(path: &str, bytes: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            size: bytes.len() as u64,
            sha256: sha256_prefixed(bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskJournal {
    schema_version: u32,
    phase: JournalPhase,
    old_selector: SelectorSnapshot,
    new_selector: SelectorSnapshot,
    generation_id: String,
    fence: Fence,
}

/// Cross-language active selector. Its on-disk bytes must be the canonical
/// JSON representation rather than merely a semantically equivalent object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveSelector {
    bundle_id: String,
    schema_version: u32,
    selector_protocol: String,
}

impl DiskJournal {
    fn from_journal(journal: &Journal) -> Self {
        Self {
            schema_version: 1,
            phase: journal.phase,
            old_selector: journal.old_selector.clone(),
            new_selector: journal.new_selector.clone(),
            generation_id: journal.generation.id.clone(),
            fence: journal.fence,
        }
    }

    fn into_journal(self, platform: &FilesystemPlatform) -> Result<Journal, TransactionError> {
        if self.schema_version != 1 {
            return Err(TransactionError::Journal(format!(
                "runtime journal schema_version is {}, expected 1",
                self.schema_version
            )));
        }
        validate_selector(&self.old_selector, TransactionError::Journal)?;
        validate_selector(&self.new_selector, TransactionError::Journal)?;
        let generation =
            platform.read_generation(&self.generation_id, TransactionError::Journal)?;
        Ok(Journal {
            phase: self.phase,
            old_selector: self.old_selector,
            new_selector: self.new_selector,
            generation,
            fence: self.fence,
        })
    }
}

fn validate_generation(
    generation: &Generation,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    if generation.executable.is_empty() {
        return Err(error(
            "generation must have a nonempty executable".to_owned(),
        ));
    }
    bundle_id_hex(&generation.id, error)?;
    if generation.projection.generation_id != generation.id {
        return Err(error(
            "host projection generation id differs from immutable generation id".to_owned(),
        ));
    }
    if generation.files.len() > MAX_GENERATION_FILES {
        return Err(error(format!(
            "generation has more than {MAX_GENERATION_FILES} files"
        )));
    }
    for path in generation.files.keys() {
        validate_relative_path(path, "generation file", error)?;
    }
    Ok(())
}

fn validate_generation_descriptor(
    descriptor: &GenerationDescriptor,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    if descriptor.files.len() > MAX_GENERATION_FILES {
        return Err(error(
            "generation metadata has invalid id or file count".to_owned(),
        ));
    }
    bundle_id_hex(&descriptor.id, error)?;
    if descriptor.projection_generation_id != descriptor.id {
        return Err(error(
            "generation metadata host projection id differs from generation id".to_owned(),
        ));
    }
    let expected_executable = GenerationFile::for_payload(GENERATION_EXECUTABLE_FILE, &[]);
    if descriptor.executable.path != expected_executable.path
        || descriptor.projection.path != GENERATION_PROJECTION_FILE
        || !is_sha256(&descriptor.executable.sha256)
        || !is_sha256(&descriptor.projection.sha256)
    {
        return Err(error(
            "generation metadata has invalid fixed payload descriptors".to_owned(),
        ));
    }
    let mut previous: Option<&str> = None;
    for file in &descriptor.files {
        validate_relative_path(&file.path, "generation metadata file", error)?;
        if !is_sha256(&file.sha256) {
            return Err(error(format!(
                "generation metadata file {} has invalid digest",
                file.path
            )));
        }
        if previous.is_some_and(|prior| prior >= file.path.as_str()) {
            return Err(error(
                "generation metadata file paths are duplicate or noncanonical".to_owned(),
            ));
        }
        previous = Some(&file.path);
    }
    Ok(())
}

fn assert_generation_entries_exact(
    directory: &Path,
    descriptor: &GenerationDescriptor,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let top_level = read_directory_names(directory, "generation directory", error)?;
    let expected_top = [
        GENERATION_EXECUTABLE_FILE,
        GENERATION_FILES_DIR,
        GENERATION_METADATA_FILE,
        GENERATION_PROJECTION_FILE,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if top_level != expected_top {
        return Err(error(
            "immutable generation contains missing or extra top-level entries".to_owned(),
        ));
    }
    let files_root = directory.join(GENERATION_FILES_DIR);
    ensure_real_directory(&files_root, "generation files directory", error)?;
    let actual = collect_relative_regular_files(&files_root, &files_root, error)?;
    let expected = descriptor
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    if actual != expected {
        return Err(error(
            "immutable generation contains missing or extra files".to_owned(),
        ));
    }
    Ok(())
}

fn read_descriptor_file(
    path: &Path,
    descriptor: &GenerationFile,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<Vec<u8>, TransactionError> {
    let bytes = read_bounded_file(
        path,
        label,
        super::outer_manifest::MAX_SINGLE_FILE_BYTES,
        error,
    )?;
    if bytes.len() as u64 != descriptor.size || sha256_prefixed(&bytes) != descriptor.sha256 {
        return Err(error(format!(
            "{label} differs from immutable generation metadata"
        )));
    }
    Ok(bytes)
}

fn validate_selector(
    selector: &SelectorSnapshot,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    if selector.bytes.len() as u64 > MAX_SELECTOR_BYTES {
        return Err(error(
            "selector exceeds the bounded selector size".to_owned(),
        ));
    }
    match &selector.generation_id {
        Some(id) => {
            let expected = canonical_selector_bytes(id, error)?;
            if selector.bytes == expected {
                Ok(())
            } else {
                Err(error(
                    "selector bytes do not exactly match the canonical active.json protocol"
                        .to_owned(),
                ))
            }
        }
        None if selector.bytes.is_empty() => Ok(()),
        None => Err(error(
            "absent selector must retain an empty selector byte snapshot".to_owned(),
        )),
    }
}

fn selector_snapshot_from_bytes(
    bytes: &[u8],
    error: fn(String) -> TransactionError,
) -> Result<SelectorSnapshot, TransactionError> {
    let selector: ActiveSelector = serde_json::from_slice(bytes)
        .map_err(|cause| error(format!("active selector is not protocol-v1 JSON: {cause}")))?;
    if selector.schema_version != 1
        || selector.selector_protocol != "epic-harness-materialized-runtime-v1"
    {
        return Err(error(
            "active selector has an unsupported schema version or selector protocol".to_owned(),
        ));
    }
    let canonical = canonical_selector_bytes(&selector.bundle_id, error)?;
    if bytes != canonical {
        return Err(error(
            "active selector JSON is not the exact canonical protocol-v1 byte sequence".to_owned(),
        ));
    }
    Ok(SelectorSnapshot::new(
        Some(selector.bundle_id),
        bytes.to_vec(),
    ))
}

fn canonical_selector_bytes(
    bundle_id: &str,
    error: fn(String) -> TransactionError,
) -> Result<Vec<u8>, TransactionError> {
    bundle_id_hex(bundle_id, error)?;
    let value = serde_json::to_value(ActiveSelector {
        bundle_id: bundle_id.to_owned(),
        schema_version: 1,
        selector_protocol: "epic-harness-materialized-runtime-v1".to_owned(),
    })
    .map_err(|cause| error(format!("cannot serialize active selector: {cause}")))?;
    super::outer_manifest::canonical_json(&value)
        .map_err(|cause| error(format!("cannot canonicalize active selector: {cause}")))
}

fn bundle_id_hex<'a>(
    bundle_id: &'a str,
    error: fn(String) -> TransactionError,
) -> Result<&'a str, TransactionError> {
    let hex = bundle_id
        .strip_prefix("sha256:")
        .ok_or_else(|| error("bundle id must use the sha256: prefix".to_owned()))?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(error(
            "bundle id must contain 64 lower-case hexadecimal sha256 digits".to_owned(),
        ));
    }
    Ok(hex)
}

fn ensure_or_create_directory(
    path: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_real_directory(&metadata) => Ok(()),
        Ok(_) => Err(error(format!(
            "path is not a regular non-symlink directory: {}",
            path.display()
        ))),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|cause| {
                error(format!(
                    "cannot create directory {}: {cause}",
                    path.display()
                ))
            })?;
            ensure_real_directory(path, "created runtime directory", error)
        }
        Err(cause) => Err(error(format!(
            "cannot inspect directory {}: {cause}",
            path.display()
        ))),
    }
}

fn ensure_or_create_regular_file(
    path: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure_real_file_metadata(&metadata, "runtime file", path, error),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => write_new_file(path, b"", error),
        Err(cause) => Err(error(format!(
            "cannot inspect runtime file {}: {cause}",
            path.display()
        ))),
    }
}

fn ensure_regular_file(
    path: &Path,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|cause| {
        error(format!(
            "cannot inspect {label} {}: {cause}",
            path.display()
        ))
    })?;
    ensure_real_file_metadata(&metadata, label, path, error)
}

fn ensure_real_directory(
    path: &Path,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|cause| {
        error(format!(
            "cannot inspect {label} {}: {cause}",
            path.display()
        ))
    })?;
    if is_real_directory(&metadata) {
        Ok(())
    } else {
        Err(error(format!(
            "{label} is not a regular non-symlink directory: {}",
            path.display()
        )))
    }
}

fn ensure_real_file_metadata(
    metadata: &Metadata,
    label: &str,
    path: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    if metadata.file_type().is_symlink() || is_reparse_point(metadata) || !metadata.is_file() {
        Err(error(format!(
            "{label} is not a regular non-symlink file: {}",
            path.display()
        )))
    } else {
        Ok(())
    }
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

fn read_bounded_file(
    path: &Path,
    label: &str,
    limit: u64,
    error: fn(String) -> TransactionError,
) -> Result<Vec<u8>, TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|cause| {
        error(format!(
            "cannot inspect {label} {}: {cause}",
            path.display()
        ))
    })?;
    ensure_real_file_metadata(&metadata, label, path, error)?;
    if metadata.len() > limit {
        return Err(error(format!(
            "{label} exceeds {limit} byte limit: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|cause| error(format!("cannot read {label} {}: {cause}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(error(format!(
            "{label} exceeded {limit} byte limit after read: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn write_new_file(
    path: &Path,
    bytes: &[u8],
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let mut file = new_file(path, error)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|cause| error(format!("cannot durably write {}: {cause}", path.display())))
}

fn new_file(path: &Path, error: fn(String) -> TransactionError) -> Result<File, TransactionError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|cause| {
        error(format!(
            "cannot create new file {}: {cause}",
            path.display()
        ))
    })
}

fn durable_replace(
    destination: &Path,
    bytes: &[u8],
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let parent = destination
        .parent()
        .ok_or_else(|| error("durable destination has no parent".to_owned()))?;
    ensure_real_directory(parent, "durable destination parent", error)?;
    let temporary = fresh_file(parent, "replace", error)?;
    let write_result = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|_| file.sync_all()));
    if let Err(cause) = write_result {
        return Err(cleanup_file_error(
            &temporary,
            error(format!(
                "cannot durably write replacement {}: {cause}",
                destination.display()
            )),
            error,
        ));
    }
    match crate::team::codex::atomic_replace_file(&temporary, destination) {
        Ok(()) => sync_directory(parent, error),
        Err(cause) => Err(cleanup_file_error(
            &temporary,
            error(format!(
                "cannot atomically replace {}: {cause}",
                destination.display()
            )),
            error,
        )),
    }
}

fn fresh_file(
    parent: &Path,
    purpose: &str,
    error: fn(String) -> TransactionError,
) -> Result<PathBuf, TransactionError> {
    for _ in 0..128 {
        let path = parent.join(format!(
            ".epic-runtime-{purpose}-{}-{}.tmp",
            std::process::id(),
            TEMPORARY_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        match new_file(&path, error) {
            Ok(_) => return Ok(path),
            Err(TransactionError::Stage(message)) if message.contains("exists") => continue,
            Err(other) => return Err(other),
        }
    }
    Err(error(
        "could not allocate a fresh durable replacement file".to_owned(),
    ))
}

fn fresh_directory(
    parent: &Path,
    purpose: &str,
    error: fn(String) -> TransactionError,
) -> Result<PathBuf, TransactionError> {
    for _ in 0..128 {
        let path = parent.join(format!(
            ".epic-runtime-{purpose}-{}-{}",
            std::process::id(),
            TEMPORARY_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(cause) => {
                return Err(error(format!(
                    "cannot create fresh directory {}: {cause}",
                    path.display()
                )));
            }
        }
    }
    Err(error(
        "could not allocate a fresh immutable generation directory".to_owned(),
    ))
}

fn create_directory_chain(
    root: &Path,
    target_parent: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    let canonical_root = crate::shared::paths::canonical_for_compare(root)
        .map_err(|cause| error(format!("cannot canonicalize generation file root: {cause}")))?;
    let target = target_parent.strip_prefix(root).map_err(|_| {
        error("generation file parent escapes the fresh generation directory".to_owned())
    })?;
    let mut current = root.to_path_buf();
    for component in target.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_real_directory(&metadata) => {}
            Ok(_) => {
                return Err(error(format!(
                    "generation directory component is not a regular directory: {}",
                    current.display()
                )));
            }
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|cause| {
                    error(format!(
                        "cannot create generation directory {}: {cause}",
                        current.display()
                    ))
                })?;
            }
            Err(cause) => {
                return Err(error(format!(
                    "cannot inspect generation directory: {cause}"
                )));
            }
        }
        let canonical_current =
            crate::shared::paths::canonical_for_compare(&current).map_err(|cause| {
                error(format!(
                    "cannot canonicalize generation directory component {}: {cause}",
                    current.display()
                ))
            })?;
        if !canonical_current.starts_with(&canonical_root) {
            return Err(error(format!(
                "generation directory component escapes the fresh root: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn relative_child(
    root: &Path,
    relative: &str,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<PathBuf, TransactionError> {
    validate_relative_path(relative, label, error)?;
    let candidate = relative
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component));
    let canonical_root = crate::shared::paths::canonical_for_compare(root)
        .map_err(|cause| error(format!("cannot canonicalize {label} root: {cause}")))?;
    if candidate.exists() {
        let canonical_candidate = crate::shared::paths::canonical_for_compare(&candidate)
            .map_err(|cause| error(format!("cannot canonicalize {label} candidate: {cause}")))?;
        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(error(format!(
                "{label} escapes generation root: {relative}"
            )));
        }
    }
    Ok(candidate)
}

fn validate_relative_path(
    path: &str,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    if path.is_empty()
        || path.len() > 4_096
        || path.starts_with('/')
        || path.contains('\\')
        || Path::new(path).is_absolute()
        || path.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || !component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        return Err(error(format!(
            "{label} has an unsafe relative path: {path}"
        )));
    }
    Ok(())
}

fn collect_relative_regular_files(
    root: &Path,
    directory: &Path,
    error: fn(String) -> TransactionError,
) -> Result<Vec<String>, TransactionError> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(directory)
        .map_err(|cause| {
            error(format!(
                "cannot read generation directory {}: {cause}",
                directory.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|cause| error(format!("cannot read generation directory entry: {cause}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|cause| {
            error(format!(
                "cannot inspect generation entry {}: {cause}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(error(format!(
                "generation contains a symlink or reparse point: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            files.extend(collect_relative_regular_files(root, &path, error)?);
        } else if metadata.is_file() {
            let root = crate::shared::paths::canonical_for_compare(root).map_err(|cause| {
                error(format!(
                    "cannot canonicalize generation files root: {cause}"
                ))
            })?;
            let path = crate::shared::paths::canonical_for_compare(&path)
                .map_err(|cause| error(format!("cannot canonicalize generation file: {cause}")))?;
            let relative = path.strip_prefix(root).map_err(|_| {
                error("generation file escapes its immutable files root".to_owned())
            })?;
            files.push(relative.to_string_lossy().replace('\\', "/"));
        } else {
            return Err(error(format!(
                "generation contains an unsupported entry: {}",
                path.display()
            )));
        }
    }
    files.sort();
    Ok(files)
}

fn read_directory_names(
    directory: &Path,
    label: &str,
    error: fn(String) -> TransactionError,
) -> Result<std::collections::BTreeSet<String>, TransactionError> {
    fs::read_dir(directory)
        .map_err(|cause| {
            error(format!(
                "cannot read {label} {}: {cause}",
                directory.display()
            ))
        })?
        .map(|entry| {
            entry
                .map_err(|cause| error(format!("cannot read {label} entry: {cause}")))?
                .file_name()
                .into_string()
                .map_err(|_| error(format!("{label} contains a non-UTF-8 entry")))
        })
        .collect()
}

fn parse_counter(bytes: &[u8]) -> Result<u64, TransactionError> {
    let text = std::str::from_utf8(bytes).map_err(|cause| {
        TransactionError::Stage(format!("runtime-bundle fence is not UTF-8: {cause}"))
    })?;
    let counter = text.strip_suffix('\n').ok_or_else(|| {
        TransactionError::Stage(
            "runtime-bundle fence must have one newline-terminated integer".to_owned(),
        )
    })?;
    if counter.is_empty()
        || counter.starts_with('0') && counter != "0"
        || !counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(TransactionError::Stage(
            "runtime-bundle fence must have one canonical unsigned integer".to_owned(),
        ));
    }
    counter.parse::<u64>().map_err(|cause| {
        TransactionError::Stage(format!("runtime-bundle fence counter is invalid: {cause}"))
    })
}

fn sync_directory(
    path: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|cause| error(format!("cannot sync directory {}: {cause}", path.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, error);
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(
    path: &Path,
    error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|cause| error(format!("cannot make staged executable runnable: {cause}")))
}

#[cfg(not(unix))]
fn make_executable(
    _path: &Path,
    _error: fn(String) -> TransactionError,
) -> Result<(), TransactionError> {
    Ok(())
}

fn cleanup_file_error(
    path: &Path,
    primary: TransactionError,
    error: fn(String) -> TransactionError,
) -> TransactionError {
    cleanup_error(primary, fs::remove_file(path), error)
}

fn cleanup_directory_error(
    path: &Path,
    primary: TransactionError,
    error: fn(String) -> TransactionError,
) -> TransactionError {
    cleanup_error(primary, fs::remove_dir_all(path), error)
}

fn cleanup_error(
    primary: TransactionError,
    cleanup: io::Result<()>,
    error: fn(String) -> TransactionError,
) -> TransactionError {
    match cleanup {
        Ok(()) => primary,
        Err(cause) => TransactionError::Multiple {
            primary: Box::new(primary),
            secondary: Box::new(error(format!("secondary cleanup failure: {cause}"))),
        },
    }
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::{ExecutableProbe, FilesystemPlatform, OWNER_BYTES, OWNER_FILE};
    use crate::runtime_bundle::{
        Candidate, Fence, Generation, HostAdapter, HostProjection, Journal, JournalPhase, Platform,
        RecoveryAction, SelectorSnapshot, TransactionError, recover,
    };
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::path::Path;

    #[test]
    fn creates_one_owned_runtime_bundle_layout() {
        let temporary = tempfile::tempdir().expect("temporary runtime storage");
        let platform = FilesystemPlatform::open(temporary.path()).expect("owned layout");
        assert_eq!(platform.root(), temporary.path());
        assert_eq!(
            fs::read(temporary.path().join(OWNER_FILE)).unwrap(),
            OWNER_BYTES
        );
        assert!(
            FilesystemPlatform::open(temporary.path()).is_ok(),
            "the exact owned marker must be idempotent"
        );
    }

    #[test]
    fn platform_instances_contend_on_the_persistent_os_repair_lock_and_issue_fresh_fences() {
        let temporary = tempfile::tempdir().unwrap();
        let mut first = platform(temporary.path());
        let mut second = platform(temporary.path());

        let first_fence = first.acquire_lock().unwrap();
        assert_eq!(second.acquire_lock(), Err(TransactionError::LockContended));
        first.release_lock(first_fence).unwrap();
        let second_fence = second.acquire_lock().unwrap();
        assert!(
            second_fence.0 > first_fence.0,
            "fences must never be reused"
        );
        second.release_lock(second_fence).unwrap();
    }

    #[test]
    fn selector_publication_replaces_the_exact_bytes_at_one_durable_selector_path() {
        let temporary = tempfile::tempdir().unwrap();
        let mut platform = platform(temporary.path());
        let generation = generation("new", b"new executable");
        let fence = stage(&mut platform, &generation);
        let selector = platform.selector_for(&generation);

        platform.publish_selector(&selector, fence).unwrap();
        assert_eq!(
            fs::read(temporary.path().join("active.json")).unwrap(),
            format!(
                "{{\"bundle_id\":\"{}\",\"schema_version\":1,\"selector_protocol\":\"epic-harness-materialized-runtime-v1\"}}",
                generation.id
            )
            .into_bytes()
        );
        assert_eq!(platform.selector_snapshot().unwrap(), selector);
        let pinned = platform.active_pinned_generation().unwrap().unwrap();
        let hex = generation.id.strip_prefix("sha256:").unwrap();
        assert_eq!(
            pinned.generation_dir,
            temporary.path().join("generations").join(hex)
        );
        assert_eq!(pinned.executable, pinned.generation_dir.join("executable"));
        assert_eq!(pinned.files_dir, pinned.generation_dir.join("files"));
        assert!(
            fs::read_dir(temporary.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("replace")),
            "selector replacement must leave no second selector or temporary commit file"
        );
        platform.release_lock(fence).unwrap();
    }

    #[test]
    fn executable_probe_failure_prevents_any_active_selector_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let mut platform =
            FilesystemPlatform::open_with_probe(temporary.path(), RejectingProbe).unwrap();
        let generation = generation("unproven", b"unproven executable");
        let fence = stage(&mut platform, &generation);

        let error = platform
            .verify_staged_executable(&generation)
            .expect_err("a failed version --json probe must prevent activation");
        assert!(error.to_string().contains("probe failed"), "{error}");
        assert!(
            !temporary.path().join("active.json").exists(),
            "no selector can be published after the failed executable proof"
        );
        platform.release_lock(fence).unwrap();
    }

    #[test]
    fn prepared_and_published_journal_recovery_survives_a_reconstructed_platform() {
        let temporary = tempfile::tempdir().unwrap();
        let generation = generation("new", b"new executable");
        {
            let mut first = platform(temporary.path());
            let fence = stage(&mut first, &generation);
            let old = first.selector_snapshot().unwrap();
            let new = first.selector_for(&generation);
            let prepared = Journal {
                phase: JournalPhase::Prepared,
                old_selector: old,
                new_selector: new.clone(),
                generation: generation.clone(),
                fence,
            };
            first.persist_journal(&prepared, fence).unwrap();
            first.publish_selector(&new, fence).unwrap();
            first
                .persist_journal(
                    &Journal {
                        phase: JournalPhase::Published,
                        ..prepared
                    },
                    fence,
                )
                .unwrap();
            first.release_lock(fence).unwrap();
        }

        let mut reconstructed = platform(temporary.path());
        assert_eq!(
            reconstructed.journal().unwrap().unwrap().phase,
            JournalPhase::Published
        );
        let recovered = recover(&mut reconstructed, &AcceptingHost).unwrap();
        assert_eq!(recovered.action, RecoveryAction::FinalizedPublished);
        assert!(reconstructed.journal().unwrap().is_none());
    }

    #[test]
    fn a_same_id_generation_mismatch_fails_without_overwriting_the_existing_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let mut platform = platform(temporary.path());
        let original = generation("same", b"first executable");
        let fence = stage(&mut platform, &original);
        let conflicting = generation("same", b"different executable");

        let error = platform
            .stage_generation(&conflicting, fence)
            .expect_err("immutable generations must reject a same-id mismatch");
        assert!(error.to_string().contains("differs"), "{error}");
        platform.verify_staged_executable(&original).unwrap();
        platform.release_lock(fence).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_open_old_generation_does_not_block_staging_or_activating_a_sibling_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let mut platform = platform(temporary.path());
        let old = generation("old", b"old executable");
        let old_fence = stage(&mut platform, &old);
        let old_selector = platform.selector_for(&old);
        platform.publish_selector(&old_selector, old_fence).unwrap();
        platform.release_lock(old_fence).unwrap();
        let _running_old_executable = File::open(
            platform
                .resolve_pinned_generation(&old.id)
                .unwrap()
                .executable,
        )
        .unwrap();

        let new = generation("new", b"new executable");
        let new_fence = stage(&mut platform, &new);
        let new_selector = platform.selector_for(&new);
        platform.publish_selector(&new_selector, new_fence).unwrap();
        platform.verify_active(&new, &new_selector).unwrap();
        platform.release_lock(new_fence).unwrap();
    }

    fn generation(id: &str, executable: &[u8]) -> Generation {
        Generation {
            id: bundle_id(id),
            executable: executable.to_vec(),
            files: BTreeMap::from([("hooks/hooks.json".to_owned(), b"{}".to_vec())]),
            projection: HostProjection {
                generation_id: bundle_id(id),
                bytes: format!("projection:{id}").into_bytes(),
            },
        }
    }

    fn stage(platform: &mut FilesystemPlatform, generation: &Generation) -> Fence {
        let fence = platform.acquire_lock().unwrap();
        platform.stage_generation(generation, fence).unwrap();
        fence
    }

    fn platform(root: &Path) -> FilesystemPlatform {
        FilesystemPlatform::open_with_probe(root, AcceptingExecutableProbe).unwrap()
    }

    fn bundle_id(label: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(label.as_bytes()))
    }

    struct AcceptingExecutableProbe;

    impl ExecutableProbe for AcceptingExecutableProbe {
        fn verify_version_json(&self, executable: &Path) -> Result<(), String> {
            executable
                .is_file()
                .then_some(())
                .ok_or_else(|| format!("missing staged executable {}", executable.display()))
        }
    }

    struct RejectingProbe;

    impl ExecutableProbe for RejectingProbe {
        fn verify_version_json(&self, _executable: &Path) -> Result<(), String> {
            Err("version --json exited unsuccessfully".to_owned())
        }
    }

    struct AcceptingHost;

    impl HostAdapter for AcceptingHost {
        fn validate_candidate(&self, _candidate: &Candidate) -> Result<(), TransactionError> {
            Ok(())
        }

        fn project_generation(
            &self,
            candidate: &Candidate,
        ) -> Result<HostProjection, TransactionError> {
            Ok(HostProjection {
                generation_id: candidate.id.clone(),
                bytes: Vec::new(),
            })
        }

        fn verify_projection(&self, _generation: &Generation) -> Result<(), TransactionError> {
            Ok(())
        }

        fn verify_active(
            &self,
            _generation: &Generation,
            _selector: &SelectorSnapshot,
        ) -> Result<(), TransactionError> {
            Ok(())
        }
    }
}
