//! Provider abstraction and data model for GitHub pull request review.
//!
//! This is the low-level layer (no `git_ui` dependency) so that both the review
//! UI and the git panel can depend on it without a dependency cycle.

mod github_provider;
mod review_provider;

pub use github_provider::*;
pub use review_provider::*;
