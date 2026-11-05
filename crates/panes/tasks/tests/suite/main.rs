//! One test binary, since each links the whole host. Each module is the Rust side of an upstream test file; see its header.

#![allow(dead_code)]

mod common;
mod helpers;
mod history_record;
mod host_commands;
mod shell_round_trip;
mod subagents;
mod task_container;
mod usage_statistics;
mod views;
mod workflow;
