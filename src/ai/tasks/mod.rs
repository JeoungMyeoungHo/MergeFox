//! Concrete AI tasks.
//!
//! Each task owns its prompt, grammar choice, and output parser.
//! Splitting them into separate files keeps the prompts close to the
//! code that consumes their output — a prompt tweak should never
//! require editing a sibling task.

pub mod commit_composer;
pub mod commit_message;
pub mod explain_change;
pub mod pr_conflict;
pub mod stash_message;
