//! barocss-hub canonical core: CRDT-native repository semantics.
//!
//! `tree` is an in-memory experiment (Issue #5) of the structural part of
//! Wiki Repository-Semantics (wiki 1292a51): no content, persistence or sync.
//! `projection` is the §1.5 filesystem projection (Issue #9), pure, no I/O.

pub mod projection;
pub mod tree;
