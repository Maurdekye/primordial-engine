//! Model v2 of WS-model (M2 S7; docs/spec/divergence.md 9.5): the merge against finality, checked on a toy world
//! before merge engine v1 is built (coordinator rulings D1, D2).
//!
//! Layer 1, here: the merge as a pure function ([`merge::merge`]), checked by exhaustive enumeration of its inputs
//! within a profile's bounds ([`enumerate`]) against properties computed from 9.2's definitions ([`props`]). Each
//! control of 9.5 is a [`merge::Mutation`] that some property must catch. Layer 2 ([`members`]): the members as a
//! Stateright model, for the vote rule (P-M3) and package r2's rules, each with its control. Layer 1b
//! ([`witness`]): the rule that decides layer 1's witnessed set, with a Byzantine signer at `n = 4`.

pub mod enumerate;
pub mod members;
pub mod merge;
pub mod props;
pub mod witness;
pub mod world;
