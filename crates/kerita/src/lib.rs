//! A subset of KERI: KERI 2.00 events in the native CESR body, read, written and folded into key
//! state, with no I/O, no storage and no secret key; the CESR primitives are `kerita-cesr`'s,
//! re-exported here. Specs: `mia-docs/openspec/specs/components/mee-pdn/kerita/`.

extern crate alloc;

pub mod annotate;
pub mod event;
#[doc(hidden)]
pub mod fuzz;
pub mod message;
pub use kerita_cesr::{primitive, signature};
pub mod state;
pub mod threshold;
