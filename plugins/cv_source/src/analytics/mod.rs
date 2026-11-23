//! What turns per-frame results into things that happen: tracks across
//! frames, rules over them (lines, zones, counts, ...), pose helpers, and
//! Lua rules. No models here, and no flwrs protocol.

pub mod assign;
pub mod control;
pub mod pose;
pub mod rules;
#[cfg(feature = "lua")]
pub mod script;
pub mod track;

#[cfg(test)]
mod stability_tests;
