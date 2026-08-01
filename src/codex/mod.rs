//! Codex cache bundle diagnosis and repair.
//!
//! This module deliberately owns no command-line parsing.  The binary wires
//! its `codex doctor` surface to [`doctor::diagnose`] and [`doctor::repair`].

pub mod bundle;
pub mod doctor;

pub use doctor::{
    CommandObservation, DiagnoseOptions, DoctorError, DoctorReport, DoctorSeams, RepairReport,
    diagnose, diagnose_with, recover_interrupted_repair, repair, repair_with,
};
