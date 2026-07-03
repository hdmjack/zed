use anyhow::Result;
use serde_json::Value;

use crate::migrations::migrate_settings;

const REVIEW_PANEL_KEY: &str = "review_panel";

/// The standalone review panel was folded into the git panel; its settings key
/// no longer exists, so drop it.
pub fn remove_review_panel(value: &mut Value) -> Result<()> {
    migrate_settings(value, &mut |object| {
        object.remove(REVIEW_PANEL_KEY);
        Ok(())
    })
}
