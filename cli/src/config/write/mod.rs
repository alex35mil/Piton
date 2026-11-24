//! Configuration into files: [`file`] is the write path (which file, the change, checked before writing); [`edit`]
//! applies key changes the way a person would, keeping comments and layout; [`layout`] lays out the arrays it writes.

pub mod edit;
pub mod file;
mod layout;
