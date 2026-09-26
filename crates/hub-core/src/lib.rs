//! barocss-hub canonical core: CRDT-native repository semantics.
//!
//! `tree` is an in-memory experiment (Issue #5) of the structural part of
//! Wiki Repository-Semantics (wiki 1292a51): no content, persistence or sync.
//! `projection` is the §1.5 filesystem projection (Issue #9), pure, no I/O.
//! `version` is the §3 version model (Issue #12): frontier, VV, compare, ⊔/⊓.
//! `checkpoint` is §3.4 (Issue #13): checkpoints (D-004), short ids (D-005).

pub mod checkpoint;
pub mod projection;
pub mod tree;
pub mod version;
