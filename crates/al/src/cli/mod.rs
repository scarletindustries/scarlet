//! Custom command-line UI. clap is used purely as an argument parser and
//! introspectable command model — every byte of user-facing output (help,
//! version, errors, man page) is rendered here, not by clap.

pub mod help;
pub mod man;
