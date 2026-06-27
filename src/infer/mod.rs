//! Faithful schema exporters: read the EXPRESS schema union and emit the codegen
//! input (`universal.toml`) and per-target output profiles (`ap*.toml`), plus the
//! shared substrate (`export_common`, `refgraph`) they build on.

#![allow(dead_code)] // some exporter helper fields are deserialized but not read

pub mod export_common;
pub mod profile_export;
pub mod refgraph;
pub mod universal_export;
