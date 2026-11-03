//! Pure domain logic: no I/O, no async, no ambient clock reads (time is injected).

pub mod benchmark;
pub mod comment;
pub mod decision;
pub mod enums;
pub mod evidence;
pub mod filtering;
pub mod jobs;
pub mod language;
pub mod qualification;
pub mod scoring;
pub mod time;

pub use enums::*;
