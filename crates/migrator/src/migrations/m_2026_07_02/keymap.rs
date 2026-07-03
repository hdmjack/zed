use collections::HashMap;
use std::{ops::Range, sync::LazyLock};
use tree_sitter::{Query, QueryMatch};

use crate::MigrationPatterns;
use crate::patterns::{KEYMAP_ACTION_STRING_PATTERN, KEYMAP_CONTEXT_PATTERN};

/// The PR review UI moved from a standalone `review_ui`/`ReviewPanel` dock into
/// the git panel's Pull Requests tab, so its actions and key contexts were
/// renamed.
pub const KEYMAP_PATTERNS: MigrationPatterns = &[
    (KEYMAP_ACTION_STRING_PATTERN, replace_string_action),
    (KEYMAP_CONTEXT_PATTERN, rename_context_key),
];

fn replace_string_action(
    contents: &str,
    mat: &QueryMatch,
    query: &Query,
) -> Option<(Range<usize>, String)> {
    let action_name_ix = query.capture_index_for_name("action_name")?;
    let action_name_node = mat.nodes_for_capture_index(action_name_ix).next()?;
    let action_name_range = action_name_node.byte_range();
    let action_name = contents.get(action_name_range.clone())?;

    let new_action_name = STRING_REPLACE.get(&action_name)?;
    Some((action_name_range, new_action_name.to_string()))
}

fn rename_context_key(
    contents: &str,
    mat: &QueryMatch,
    query: &Query,
) -> Option<(Range<usize>, String)> {
    let context_predicate_ix = query.capture_index_for_name("context_predicate")?;
    let context_predicate_range = mat
        .nodes_for_capture_index(context_predicate_ix)
        .next()?
        .byte_range();
    let old_predicate = contents.get(context_predicate_range.clone())?.to_string();
    let mut new_predicate = old_predicate.clone();

    const REPLACEMENTS: &[(&str, &str)] = &[
        ("ReviewComposer", "PullRequestComposer"),
        ("ReviewPanel", "PullRequestPanel"),
    ];

    for (old, new) in REPLACEMENTS {
        new_predicate = new_predicate.replace(old, new);
    }

    if new_predicate != old_predicate {
        Some((context_predicate_range, new_predicate))
    } else {
        None
    }
}

static STRING_REPLACE: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
    HashMap::from_iter([
        ("review_ui::AddComment", "pull_request::AddComment"),
        ("review_ui::SubmitComment", "pull_request::SubmitComment"),
        ("review_ui::ToggleViewed", "pull_request::ToggleViewed"),
        // The Pull Requests tab is now a git panel tab.
        (
            "review_ui::ActivatePullRequestsTab",
            "git_panel::ActivatePullRequestsTab",
        ),
    ])
});
