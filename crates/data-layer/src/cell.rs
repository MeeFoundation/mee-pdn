//! A cell's two stores, the membership store and the record store, by the
//! cell stores spec.

mod keys;

pub use keys::{record_prefix, EventKind, MembershipKey, OpId, RecordKey, Seq};
