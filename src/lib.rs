//! Declarative VS Code profile management.
//!
//! VS Code profiles do not inherit — "New Profile from Default" copies once — so a shared base set
//! duplicated by hand into every profile drifts. This crate treats profiles as build artifacts
//! generated from a TOML manifest.
//!
//! The split that matters: [`state`], [`store`] and [`manifest`] only read; [`plan`] is a pure
//! function over their output; [`apply`] is the only module that mutates anything, and it does so
//! exclusively through the `code` CLI.

pub mod apply;
pub mod backup;
pub mod classify;
pub mod export;
pub mod guard;
pub mod manifest;
pub mod paths;
pub mod plan;
pub mod restore;
pub mod state;
pub mod store;
