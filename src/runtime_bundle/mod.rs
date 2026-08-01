//! Host-neutral immutable runtime-bundle transactions.
//!
//! The state machine in this module only knows about the seams represented by
//! [`Platform`] and [`HostAdapter`].  Filesystem, process, and host-specific
//! policy belong in those adapters.  A transaction is consistent for its
//! invocation; this module does not claim global live atomicity.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub mod filesystem;
pub mod outer_manifest;

pub use filesystem::{ExecutableProbe, FilesystemPlatform, PinnedGeneration};
pub use outer_manifest::{CandidateLoadOptions, LoadedCandidate, OuterManifest, load_candidate};

/// An immutable candidate to install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub executable: Vec<u8>,
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Candidate {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            executable: b"executable".to_vec(),
            files: BTreeMap::new(),
        }
    }

    pub fn with_executable(mut self, executable: impl Into<Vec<u8>>) -> Self {
        self.executable = executable.into();
        self
    }

    pub fn with_file(mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.files.insert(name.into(), bytes.into());
        self
    }
}

/// A host projection stored alongside a generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProjection {
    pub generation_id: String,
    pub bytes: Vec<u8>,
}

/// A complete staged generation.  Once passed to [`Platform::stage_generation`]
/// it is treated as immutable by the transaction core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation {
    pub id: String,
    pub executable: Vec<u8>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub projection: HostProjection,
}

/// The exact selector bytes observed before a transaction starts or written by
/// a transaction.  Keeping the bytes makes rollback a snapshot operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorSnapshot {
    pub generation_id: Option<String>,
    pub bytes: Vec<u8>,
}

impl SelectorSnapshot {
    pub fn new(generation_id: Option<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            generation_id,
            bytes: bytes.into(),
        }
    }
}

/// A fence issued by an exclusive lock acquisition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fence(pub u64);

/// Journal phases are ordered: a prepared journal has not recorded selector
/// publication; a published journal has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalPhase {
    Prepared,
    Published,
}

/// Durable transaction record used by recovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub phase: JournalPhase,
    pub old_selector: SelectorSnapshot,
    pub new_selector: SelectorSnapshot,
    pub generation: Generation,
    pub fence: Fence,
}

/// Errors returned by the transaction seams and state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionError {
    InvalidCandidate(String),
    LockContended,
    FenceMismatch,
    Stage(String),
    Verification(String),
    Journal(String),
    Publish(String),
    PostActivation(String),
    Rollback(String),
    Release(String),
    Recovery(String),
    Multiple {
        primary: Box<TransactionError>,
        secondary: Box<TransactionError>,
    },
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCandidate(message) => write!(f, "invalid candidate: {message}"),
            Self::LockContended => f.write_str("exclusive lock is held"),
            Self::FenceMismatch => f.write_str("lock fence mismatch"),
            Self::Stage(message) => write!(f, "stage failed: {message}"),
            Self::Verification(message) => write!(f, "verification failed: {message}"),
            Self::Journal(message) => write!(f, "journal failed: {message}"),
            Self::Publish(message) => write!(f, "selector publication failed: {message}"),
            Self::PostActivation(message) => {
                write!(f, "post-activation verification failed: {message}")
            }
            Self::Rollback(message) => write!(f, "rollback failed: {message}"),
            Self::Release(message) => write!(f, "lock release failed: {message}"),
            Self::Recovery(message) => write!(f, "recovery failed: {message}"),
            Self::Multiple { primary, secondary } => {
                write!(f, "{primary}; secondary failure: {secondary}")
            }
        }
    }
}

impl std::error::Error for TransactionError {}

/// The platform seam.  Implementations own storage, durability, locking, and
/// process probes.  Every mutating method receives the fence acquired for the
/// current invocation.
pub trait Platform {
    fn acquire_lock(&mut self) -> Result<Fence, TransactionError>;
    fn release_lock(&mut self, fence: Fence) -> Result<(), TransactionError>;

    fn selector_snapshot(&self) -> Result<SelectorSnapshot, TransactionError>;

    fn stage_generation(
        &mut self,
        generation: &Generation,
        fence: Fence,
    ) -> Result<(), TransactionError>;
    fn verify_staged_executable(&self, generation: &Generation) -> Result<(), TransactionError>;

    fn selector_for(&self, generation: &Generation) -> SelectorSnapshot {
        SelectorSnapshot::new(
            Some(generation.id.clone()),
            generation.id.as_bytes().to_vec(),
        )
    }

    fn persist_journal(&mut self, journal: &Journal, fence: Fence) -> Result<(), TransactionError>;
    fn journal(&self) -> Result<Option<Journal>, TransactionError>;
    fn clear_journal(&mut self, fence: Fence) -> Result<(), TransactionError>;

    /// Publishes the new selector.  The core calls this at most once per
    /// forward transaction; rollback uses the separate rollback seam.
    fn publish_selector(
        &mut self,
        selector: &SelectorSnapshot,
        fence: Fence,
    ) -> Result<(), TransactionError>;
    fn rollback_selector(
        &mut self,
        selector: &SelectorSnapshot,
        fence: Fence,
    ) -> Result<(), TransactionError>;

    fn verify_active(
        &self,
        generation: &Generation,
        selector: &SelectorSnapshot,
    ) -> Result<(), TransactionError>;
}

/// The host seam.  Host adapters validate candidates and produce/verify their
/// host projection.  They do not own selector or journal mutation.
pub trait HostAdapter {
    fn validate_candidate(&self, candidate: &Candidate) -> Result<(), TransactionError>;
    fn project_generation(&self, candidate: &Candidate)
    -> Result<HostProjection, TransactionError>;
    fn verify_projection(&self, generation: &Generation) -> Result<(), TransactionError>;
    fn verify_active(
        &self,
        generation: &Generation,
        selector: &SelectorSnapshot,
    ) -> Result<(), TransactionError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub candidate_valid: bool,
    pub validation_error: Option<String>,
    pub selector: SelectorSnapshot,
    pub journal: Option<Journal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    pub old_selector: SelectorSnapshot,
    pub new_selector: SelectorSnapshot,
    pub generation_id: String,
    pub fence: Fence,
    pub selector_published: bool,
    pub rolled_back: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    NothingToDo,
    ClearedPrepared,
    FinalizedPublished,
    RolledBack,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub action: RecoveryAction,
    pub selector: SelectorSnapshot,
    pub journal_retained: bool,
    pub fence: Fence,
}

/// Read-only diagnosis.  Candidate validation is reported, not mutated into
/// platform state.
pub fn diagnose<P: Platform, H: HostAdapter>(
    platform: &P,
    host: &H,
    candidate: &Candidate,
) -> Result<DiagnosticReport, TransactionError> {
    let validation_error = match host.validate_candidate(candidate) {
        Ok(()) => None,
        Err(error) => Some(error),
    };
    Ok(DiagnosticReport {
        candidate_valid: validation_error.is_none(),
        validation_error: validation_error.map(|error| error.to_string()),
        selector: platform.selector_snapshot()?,
        journal: platform.journal()?,
    })
}

/// Validate, stage, publish, verify, and release one exclusive transaction.
pub fn repair_and_activate<P: Platform, H: HostAdapter>(
    platform: &mut P,
    host: &H,
    candidate: &Candidate,
) -> Result<RepairReport, TransactionError> {
    // This call is intentionally before acquire_lock: invalid input must not
    // mutate a lock, selector, generation, or journal.
    host.validate_candidate(candidate)?;

    let fence = platform.acquire_lock()?;
    let result = repair_locked(platform, host, candidate, fence);
    let release = platform.release_lock(fence);
    match (result, release) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(TransactionError::Multiple {
            primary: Box::new(error),
            secondary: Box::new(release_error),
        }),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn repair_locked<P: Platform, H: HostAdapter>(
    platform: &mut P,
    host: &H,
    candidate: &Candidate,
    fence: Fence,
) -> Result<RepairReport, TransactionError> {
    let old_selector = platform.selector_snapshot()?;
    let projection = host.project_generation(candidate)?;
    let generation = Generation {
        id: candidate.id.clone(),
        executable: candidate.executable.clone(),
        files: candidate.files.clone(),
        projection,
    };

    platform.stage_generation(&generation, fence)?;
    platform.verify_staged_executable(&generation)?;
    host.verify_projection(&generation)?;

    let new_selector = platform.selector_for(&generation);
    let prepared = Journal {
        phase: JournalPhase::Prepared,
        old_selector: old_selector.clone(),
        new_selector: new_selector.clone(),
        generation: generation.clone(),
        fence,
    };
    platform.persist_journal(&prepared, fence)?;

    match platform.publish_selector(&new_selector, fence) {
        Ok(()) => {}
        Err(error) => return handle_publish_error(platform, &prepared, error, fence),
    }

    let published = Journal {
        phase: JournalPhase::Published,
        ..prepared.clone()
    };
    // If this persistence fails the selector is already a complete new value;
    // leave it selected and return the durable-journal error for the caller.
    if let Err(error) = platform.persist_journal(&published, fence) {
        return rollback_after_post_verify(platform, &published, error, fence);
    }

    if let Err(error) = platform.verify_active(&generation, &new_selector) {
        return rollback_after_post_verify(platform, &published, error, fence);
    }
    if let Err(error) = host.verify_active(&generation, &new_selector) {
        return rollback_after_post_verify(platform, &published, error, fence);
    }

    platform.clear_journal(fence)?;
    Ok(RepairReport {
        old_selector,
        new_selector,
        generation_id: generation.id,
        fence,
        selector_published: true,
        rolled_back: false,
    })
}

fn combine_errors(
    primary: TransactionError,
    first_secondary: Option<TransactionError>,
    second_secondary: Option<TransactionError>,
) -> TransactionError {
    [first_secondary, second_secondary]
        .into_iter()
        .flatten()
        .fold(primary, |primary, secondary| TransactionError::Multiple {
            primary: Box::new(primary),
            secondary: Box::new(secondary),
        })
}

fn handle_publish_error<P: Platform>(
    platform: &mut P,
    prepared: &Journal,
    error: TransactionError,
    fence: Fence,
) -> Result<RepairReport, TransactionError> {
    // A seam may report after changing the selector.  Inspect the snapshot and
    // normalize either outcome to a complete old or complete new selection.
    let current = match platform.selector_snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => return Err(error),
    };
    if current == prepared.new_selector {
        let published = Journal {
            phase: JournalPhase::Published,
            ..prepared.clone()
        };
        let persist_error = match platform.persist_journal(&published, fence) {
            Ok(()) => None,
            Err(error) => Some(error),
        };
        match platform.rollback_selector(&prepared.old_selector, fence) {
            Ok(()) => {
                let clear_error = match platform.clear_journal(fence) {
                    Ok(()) => None,
                    Err(error) => Some(error),
                };
                Err(combine_errors(error, persist_error, clear_error))
            }
            Err(rollback) => Err(combine_errors(error, persist_error, Some(rollback))),
        }
    } else {
        Err(error)
    }
}

fn rollback_after_post_verify<P: Platform>(
    platform: &mut P,
    published: &Journal,
    post_error: TransactionError,
    fence: Fence,
) -> Result<RepairReport, TransactionError> {
    match platform.rollback_selector(&published.old_selector, fence) {
        Ok(()) => match platform.clear_journal(fence) {
            Ok(()) => Err(post_error),
            Err(clear_error) => Err(combine_errors(post_error, None, Some(clear_error))),
        },
        Err(rollback) => {
            // The published journal remains the durable recovery authority.
            Err(combine_errors(post_error, None, Some(rollback)))
        }
    }
}

/// Recover an interrupted transaction using the durable journal phase.
pub fn recover<P: Platform, H: HostAdapter>(
    platform: &mut P,
    host: &H,
) -> Result<RecoveryReport, TransactionError> {
    let fence = platform.acquire_lock()?;
    let result = recover_locked(platform, host, fence);
    let release = platform.release_lock(fence);
    match (result, release) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(TransactionError::Multiple {
            primary: Box::new(error),
            secondary: Box::new(release_error),
        }),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn recover_locked<P: Platform, H: HostAdapter>(
    platform: &mut P,
    host: &H,
    fence: Fence,
) -> Result<RecoveryReport, TransactionError> {
    let Some(journal) = platform.journal()? else {
        return Ok(RecoveryReport {
            action: RecoveryAction::NothingToDo,
            selector: platform.selector_snapshot()?,
            journal_retained: false,
            fence,
        });
    };

    let current = platform.selector_snapshot()?;
    let selected_new = current == journal.new_selector;
    let selected_old = current == journal.old_selector;

    match journal.phase {
        JournalPhase::Prepared if !selected_new => {
            // Staging is immutable and harmless to retain.  The old selector
            // remains authoritative, so a prepared record can be cleared.
            platform.clear_journal(fence)?;
            Ok(RecoveryReport {
                action: RecoveryAction::ClearedPrepared,
                selector: current,
                journal_retained: false,
                fence,
            })
        }
        JournalPhase::Prepared | JournalPhase::Published if selected_new => {
            let platform_ok = platform
                .verify_active(&journal.generation, &journal.new_selector)
                .is_ok();
            let host_ok = host
                .verify_active(&journal.generation, &journal.new_selector)
                .is_ok();
            if platform_ok && host_ok {
                platform.clear_journal(fence)?;
                Ok(RecoveryReport {
                    action: RecoveryAction::FinalizedPublished,
                    selector: current,
                    journal_retained: false,
                    fence,
                })
            } else {
                rollback_recovery(platform, &journal, fence)
            }
        }
        JournalPhase::Published if selected_old => {
            platform.clear_journal(fence)?;
            Ok(RecoveryReport {
                action: RecoveryAction::RolledBack,
                selector: current,
                journal_retained: false,
                fence,
            })
        }
        JournalPhase::Prepared | JournalPhase::Published => {
            rollback_recovery(platform, &journal, fence)
        }
    }
}

fn rollback_recovery<P: Platform>(
    platform: &mut P,
    journal: &Journal,
    fence: Fence,
) -> Result<RecoveryReport, TransactionError> {
    platform
        .rollback_selector(&journal.old_selector, fence)
        .map_err(|error| {
            // Do not clear the journal: it is the only durable recovery authority.
            match error {
                TransactionError::Rollback(_) => error,
                other => TransactionError::Recovery(other.to_string()),
            }
        })?;
    platform.clear_journal(fence)?;
    Ok(RecoveryReport {
        action: RecoveryAction::RolledBack,
        selector: journal.old_selector.clone(),
        journal_retained: false,
        fence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn candidate(id: &str) -> Candidate {
        Candidate::new(id).with_file("manifest", b"manifest".to_vec())
    }

    #[test]
    fn combined_transaction_errors_retain_every_secondary_failure() {
        let error = combine_errors(
            TransactionError::Publish("primary".to_owned()),
            Some(TransactionError::Journal("persist".to_owned())),
            Some(TransactionError::Rollback("restore".to_owned())),
        );
        let rendered = error.to_string();
        assert!(rendered.contains("primary"), "{rendered}");
        assert!(rendered.contains("persist"), "{rendered}");
        assert!(rendered.contains("restore"), "{rendered}");
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FailPoint {
        None,
        Acquire,
        Stage,
        VerifyStage,
        JournalPrepared,
        PublishBefore,
        PublishAfter,
        JournalPublished,
        Rollback,
        ClearJournal,
    }

    struct FakePlatform {
        selector: SelectorSnapshot,
        staged: Option<Generation>,
        journal: Option<Journal>,
        next_fence: u64,
        held: Option<Fence>,
        fail: FailPoint,
        selector_replace_calls: usize,
        publish_calls: usize,
        rollback_calls: usize,
        acquire_calls: usize,
        release_calls: usize,
    }

    impl FakePlatform {
        fn with_active(id: &str) -> Self {
            Self {
                selector: SelectorSnapshot::new(Some(id.to_owned()), format!("selector:{id}")),
                staged: None,
                journal: None,
                next_fence: 0,
                held: None,
                fail: FailPoint::None,
                selector_replace_calls: 0,
                publish_calls: 0,
                rollback_calls: 0,
                acquire_calls: 0,
                release_calls: 0,
            }
        }

        fn selector_bytes(&self) -> Vec<u8> {
            self.selector.bytes.clone()
        }

        fn selector_replace_calls(&self) -> usize {
            self.selector_replace_calls
        }

        fn set_fail(&mut self, fail: FailPoint) {
            self.fail = fail;
        }

        fn check_fence(&self, fence: Fence) -> Result<(), TransactionError> {
            (self.held == Some(fence))
                .then_some(())
                .ok_or(TransactionError::FenceMismatch)
        }
    }

    impl Platform for FakePlatform {
        fn acquire_lock(&mut self) -> Result<Fence, TransactionError> {
            self.acquire_calls += 1;
            if self.fail == FailPoint::Acquire {
                return Err(TransactionError::LockContended);
            }
            if self.held.is_some() {
                return Err(TransactionError::LockContended);
            }
            self.next_fence += 1;
            let fence = Fence(self.next_fence);
            self.held = Some(fence);
            Ok(fence)
        }

        fn release_lock(&mut self, fence: Fence) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            self.held = None;
            self.release_calls += 1;
            Ok(())
        }

        fn selector_snapshot(&self) -> Result<SelectorSnapshot, TransactionError> {
            Ok(self.selector.clone())
        }

        fn stage_generation(
            &mut self,
            generation: &Generation,
            fence: Fence,
        ) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            if self.fail == FailPoint::Stage {
                return Err(TransactionError::Stage("injected".into()));
            }
            self.staged = Some(generation.clone());
            Ok(())
        }

        fn verify_staged_executable(
            &self,
            _generation: &Generation,
        ) -> Result<(), TransactionError> {
            if self.fail == FailPoint::VerifyStage {
                Err(TransactionError::Verification("staged executable".into()))
            } else {
                Ok(())
            }
        }

        fn selector_for(&self, generation: &Generation) -> SelectorSnapshot {
            SelectorSnapshot::new(
                Some(generation.id.clone()),
                format!("selector:{}", generation.id).into_bytes(),
            )
        }

        fn persist_journal(
            &mut self,
            journal: &Journal,
            fence: Fence,
        ) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            if (journal.phase == JournalPhase::Prepared && self.fail == FailPoint::JournalPrepared)
                || (journal.phase == JournalPhase::Published
                    && self.fail == FailPoint::JournalPublished)
            {
                return Err(TransactionError::Journal("injected".into()));
            }
            self.journal = Some(journal.clone());
            Ok(())
        }

        fn journal(&self) -> Result<Option<Journal>, TransactionError> {
            Ok(self.journal.clone())
        }

        fn clear_journal(&mut self, fence: Fence) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            if self.fail == FailPoint::ClearJournal {
                return Err(TransactionError::Journal("clear injected".into()));
            }
            self.journal = None;
            Ok(())
        }

        fn publish_selector(
            &mut self,
            selector: &SelectorSnapshot,
            fence: Fence,
        ) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            self.publish_calls += 1;
            if self.fail == FailPoint::PublishBefore {
                return Err(TransactionError::Publish("before".into()));
            }
            self.selector = selector.clone();
            self.selector_replace_calls += 1;
            if self.fail == FailPoint::PublishAfter {
                return Err(TransactionError::Publish("after".into()));
            }
            Ok(())
        }

        fn rollback_selector(
            &mut self,
            selector: &SelectorSnapshot,
            fence: Fence,
        ) -> Result<(), TransactionError> {
            self.check_fence(fence)?;
            self.rollback_calls += 1;
            if self.fail == FailPoint::Rollback {
                return Err(TransactionError::Rollback("injected".into()));
            }
            self.selector = selector.clone();
            self.selector_replace_calls += 1;
            Ok(())
        }

        fn verify_active(
            &self,
            generation: &Generation,
            selector: &SelectorSnapshot,
        ) -> Result<(), TransactionError> {
            if self.selector != *selector || self.staged.as_ref() != Some(generation) {
                return Err(TransactionError::Verification("active selector".into()));
            }
            Ok(())
        }
    }

    struct FakeHost {
        required_file: Option<String>,
        post_fail: Cell<bool>,
        project_tag: &'static [u8],
    }

    impl FakeHost {
        fn with_missing_file(_id: &str, file: &str) -> Self {
            Self {
                required_file: Some(file.to_owned()),
                post_fail: Cell::new(false),
                project_tag: b"one",
            }
        }
    }

    impl HostAdapter for FakeHost {
        fn validate_candidate(&self, candidate: &Candidate) -> Result<(), TransactionError> {
            if candidate.id.is_empty() {
                return Err(TransactionError::InvalidCandidate("empty id".into()));
            }
            if candidate.executable.is_empty() {
                return Err(TransactionError::InvalidCandidate(
                    "empty executable".into(),
                ));
            }
            if let Some(file) = &self.required_file {
                if !candidate.files.contains_key(file) {
                    return Err(TransactionError::InvalidCandidate(format!(
                        "missing manifest file {file}"
                    )));
                }
            }
            Ok(())
        }

        fn project_generation(
            &self,
            candidate: &Candidate,
        ) -> Result<HostProjection, TransactionError> {
            Ok(HostProjection {
                generation_id: candidate.id.clone(),
                bytes: [self.project_tag, candidate.id.as_bytes()].concat(),
            })
        }

        fn verify_projection(&self, generation: &Generation) -> Result<(), TransactionError> {
            (generation.projection.generation_id == generation.id)
                .then_some(())
                .ok_or_else(|| TransactionError::Verification("host projection".into()))
        }

        fn verify_active(
            &self,
            _generation: &Generation,
            _selector: &SelectorSnapshot,
        ) -> Result<(), TransactionError> {
            if self.post_fail.get() {
                Err(TransactionError::PostActivation("injected".into()))
            } else {
                Ok(())
            }
        }
    }

    struct FakeHostTwo {
        post_fail: Cell<bool>,
    }

    impl HostAdapter for FakeHostTwo {
        fn validate_candidate(&self, candidate: &Candidate) -> Result<(), TransactionError> {
            (!candidate.id.is_empty() && !candidate.executable.is_empty())
                .then_some(())
                .ok_or_else(|| TransactionError::InvalidCandidate("invalid".into()))
        }

        fn project_generation(
            &self,
            candidate: &Candidate,
        ) -> Result<HostProjection, TransactionError> {
            Ok(HostProjection {
                generation_id: candidate.id.clone(),
                bytes: candidate.id.as_bytes().to_vec(),
            })
        }

        fn verify_projection(&self, generation: &Generation) -> Result<(), TransactionError> {
            (generation.projection.generation_id == generation.id)
                .then_some(())
                .ok_or_else(|| TransactionError::Verification("projection".into()))
        }

        fn verify_active(
            &self,
            _generation: &Generation,
            _selector: &SelectorSnapshot,
        ) -> Result<(), TransactionError> {
            (!self.post_fail.get())
                .then_some(())
                .ok_or_else(|| TransactionError::PostActivation("injected".into()))
        }
    }

    #[test]
    fn candidate_validation_precedes_selector_mutation() {
        let mut platform = FakePlatform::with_active("old");
        let before = platform.selector_bytes();
        let host = FakeHost::with_missing_file("new", "runner");
        let candidate = candidate("new");

        let error = repair_and_activate(&mut platform, &host, &candidate).unwrap_err();

        assert!(error.to_string().contains("missing manifest file runner"));
        assert_eq!(platform.selector_replace_calls(), 0);
        assert_eq!(platform.selector_bytes(), before);
        assert_eq!(platform.acquire_calls, 0);
    }

    #[test]
    fn lock_contention_and_fence_are_enforced() {
        let mut platform = FakePlatform::with_active("old");
        let fence = platform.acquire_lock().unwrap();
        assert_eq!(
            platform.acquire_lock(),
            Err(TransactionError::LockContended)
        );
        assert_eq!(
            platform.release_lock(Fence(fence.0 + 1)),
            Err(TransactionError::FenceMismatch)
        );
        assert_eq!(platform.release_lock(fence), Ok(()));
    }

    #[test]
    fn failpoint_before_publication_keeps_old_selector() {
        let mut platform = FakePlatform::with_active("old");
        platform.set_fail(FailPoint::PublishBefore);
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        let old = platform.selector_bytes();
        assert!(repair_and_activate(&mut platform, &host, &candidate("new")).is_err());
        assert_eq!(platform.selector_bytes(), old);
        assert!(platform.journal.is_some());
        assert_eq!(platform.publish_calls, 1);
    }

    #[test]
    fn failpoint_after_publication_rolls_back_to_old_selector() {
        let mut platform = FakePlatform::with_active("old");
        platform.set_fail(FailPoint::PublishAfter);
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        assert!(repair_and_activate(&mut platform, &host, &candidate("new")).is_err());
        assert_eq!(platform.selector.generation_id.as_deref(), Some("old"));
        assert_eq!(platform.publish_calls, 1);
    }

    #[test]
    fn post_verify_failure_rolls_back_and_clears_journal() {
        let mut platform = FakePlatform::with_active("old");
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(true),
            project_tag: b"one",
        };
        let error = repair_and_activate(&mut platform, &host, &candidate("new")).unwrap_err();
        assert!(matches!(error, TransactionError::PostActivation(_)));
        assert_eq!(platform.selector.generation_id.as_deref(), Some("old"));
        assert!(platform.journal.is_none());
    }

    #[test]
    fn rollback_failure_retains_published_journal() {
        let mut platform = FakePlatform::with_active("old");
        platform.set_fail(FailPoint::Rollback);
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(true),
            project_tag: b"one",
        };
        assert!(matches!(
            repair_and_activate(&mut platform, &host, &candidate("new")),
            Err(TransactionError::Multiple { .. })
        ));
        assert_eq!(platform.selector.generation_id.as_deref(), Some("new"));
        assert_eq!(
            platform.journal.as_ref().map(|journal| journal.phase),
            Some(JournalPhase::Published)
        );
    }

    #[test]
    fn recovery_finalizes_published_and_clears_journal() {
        let mut platform = FakePlatform::with_active("old");
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        let old_selector = platform.selector_snapshot().unwrap();
        repair_and_activate(&mut platform, &host, &candidate("new")).unwrap();
        let generation = platform.staged.clone().unwrap();
        let new_selector = platform.selector_snapshot().unwrap();
        platform.journal = Some(Journal {
            phase: JournalPhase::Published,
            old_selector,
            new_selector,
            generation,
            fence: Fence(1),
        });
        let report = recover(&mut platform, &host).unwrap();
        assert_eq!(report.action, RecoveryAction::FinalizedPublished);
        assert!(platform.journal.is_none());
        assert_eq!(platform.selector.generation_id.as_deref(), Some("new"));
    }

    #[test]
    fn recovery_clears_prepared_journal_and_keeps_old_generation() {
        let mut platform = FakePlatform::with_active("old");
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        platform.set_fail(FailPoint::PublishBefore);
        assert!(repair_and_activate(&mut platform, &host, &candidate("new")).is_err());
        platform.set_fail(FailPoint::None);
        let report = recover(&mut platform, &host).unwrap();
        assert_eq!(report.action, RecoveryAction::ClearedPrepared);
        assert_eq!(platform.selector.generation_id.as_deref(), Some("old"));
        assert!(platform.staged.is_some());
    }

    #[test]
    fn host_adapters_follow_the_same_core_path() {
        let candidate = candidate("new");
        let mut first = FakePlatform::with_active("old");
        let host_one = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        let one = repair_and_activate(&mut first, &host_one, &candidate).unwrap();

        let mut second = FakePlatform::with_active("old");
        let host_two = FakeHostTwo {
            post_fail: Cell::new(false),
        };
        let two = repair_and_activate(&mut second, &host_two, &candidate).unwrap();

        assert_eq!(one.generation_id, two.generation_id);
        assert_eq!(first.selector, second.selector);
        assert_eq!(first.publish_calls, 1);
        assert_eq!(second.publish_calls, 1);
    }

    #[test]
    fn selector_snapshot_is_byte_exact() {
        let mut platform = FakePlatform::with_active("old");
        let before = platform.selector_snapshot().unwrap();
        let host = FakeHost {
            required_file: None,
            post_fail: Cell::new(false),
            project_tag: b"one",
        };
        repair_and_activate(&mut platform, &host, &candidate("new")).unwrap();
        assert_eq!(before.bytes, b"selector:old".to_vec());
        assert_eq!(platform.selector.bytes, b"selector:new".to_vec());
    }

    #[test]
    fn diagnosis_is_read_only_and_reports_invalid_candidate() {
        let platform = FakePlatform::with_active("old");
        let host = FakeHost::with_missing_file("new", "runner");
        let report = diagnose(&platform, &host, &candidate("new")).unwrap();
        assert!(!report.candidate_valid);
        assert_eq!(report.selector.bytes, b"selector:old".to_vec());
    }
}
