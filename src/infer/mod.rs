//! Algorithmic schema → IR classification pipeline.
//!
//! Three stages (variant → arena → pool), each a pure function over the
//! schema union plus its overrides file. This module hosts shared types;
//! per-stage logic lives in sibling modules.

#![allow(dead_code)] // wired up incrementally across stages

pub mod arena;
pub mod export_common;
pub mod io;
pub mod l1_export;
pub mod universal_export;
pub mod naming;
pub mod overrides;
pub mod pool;
pub mod profile_export;
pub mod prune;
pub mod refgraph;
pub mod reshape;
pub mod shape;
pub mod variant;

use serde::{Deserialize, Serialize};

/// 0.0 – 1.0 confidence score. Wraps a bare `f32` so call sites can't
/// accidentally feed in a non-confidence float.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(pub f32);

impl Confidence {
    pub fn new(x: f32) -> Self {
        Self(x.clamp(0.0, 1.0))
    }

    pub fn bucket(self) -> Bucket {
        match self.0 {
            x if x >= 0.8 => Bucket::Confident,
            x if x >= 0.5 => Bucket::Review,
            _ => Bucket::Unresolved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Confident,
    Review,
    Unresolved,
}

/// How a decision arrived at its current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// Algorithm produced this without user intervention.
    Auto,
    /// User explicitly set this via the overrides file (`[entity.X] ...`).
    Override,
    /// User accepted the auto decision (legacy variant — no current stage
    /// emits this since the bulk-accept mechanism was removed alongside
    /// arena's 3-bucket scaffolding).
    Accepted,
}

/// One classification decision — emitted to either `<stage>.toml` (when
/// confident) or `<stage>_pending.toml` (when review or unresolved).
///
/// `T` is the stage-specific payload (`VariantSpec` for variant stage,
/// future `ArenaSpec`). For unresolved decisions the payload
/// can be a placeholder (`VariantSpec::default()` or similar) since the
/// caller relies on `bucket()` to know it's not a real decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision<T> {
    #[serde(flatten)]
    pub data: T,
    pub source: DecisionSource,
    pub confidence: Confidence,
    /// Human-readable explanations of the signals that produced this
    /// decision. Written into `<stage>_pending.toml` for review /
    /// unresolved entries; suppressed in `<stage>.toml` to keep confident
    /// output minimal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

impl<T> Decision<T> {
    pub fn bucket(&self) -> Bucket {
        self.confidence.bucket()
    }
}

/// Outcome of running one stage. Caller writes the confident map to
/// `<stage>.toml` (always) and the pending sections to
/// `<stage>_pending.toml` (only when non-empty — empty pending → file
/// deleted).
pub struct InferResult<T> {
    /// Confident decisions, keyed by the unit name (entity for variant,
    /// group for arena, arena for pool). Sorted by `BTreeMap` for
    /// deterministic output.
    pub confident: std::collections::BTreeMap<String, Decision<T>>,
    /// Review decisions — auto-decided but signal weak. Stay in pending
    /// until accepted or overridden.
    pub review: std::collections::BTreeMap<String, Decision<T>>,
    /// Unresolved — algorithm couldn't decide. Override mandatory.
    pub unresolved: std::collections::BTreeMap<String, Unresolved>,
}

/// An unresolved entry. Carries enough context for the user to write an
/// explicit override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unresolved {
    pub reasons: Vec<String>,
    /// Suggested override snippet (TOML body) to paste into the overrides
    /// file. Stage-specific.
    pub override_example: String,
}
