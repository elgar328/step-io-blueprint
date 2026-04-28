//! Algorithmic schema → IR classification pipeline.
//!
//! Three stages (variant → arena → pool), each a pure function over the
//! schema union plus its overrides file. This module hosts shared types;
//! per-stage logic lives in sibling modules.

#![allow(dead_code)] // wired up incrementally across stages

pub mod refgraph;
