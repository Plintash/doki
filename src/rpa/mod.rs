//! The debug RPA bridge.
//!
//! A debug build of the app exposes a small local endpoint that answers a
//! semantic snapshot of what is on screen, acts on controls the way a user
//! would, and records enough runtime evidence to debug what a snapshot cannot
//! explain. It exists so feature work can be verified programmatically
//! instead of through pixels or the system accessibility tree.
//!
//! See `openspec/changes/add-rpa-bridge/` for the behavior contract.

pub mod protocol;
pub mod service;
