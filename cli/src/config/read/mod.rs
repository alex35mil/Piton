//! Configuration from files: [`parse`] reads one file into its parts, [`resolve`] a chain into one merged value or
//! error per part, and [`global`] the global tier's tables the daemon and the apps use.

pub mod global;
pub mod parse;
pub mod resolve;
