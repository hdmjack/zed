mod comment_card;
mod file_list;
mod github_provider;
mod github_token;
mod inline_comment;
mod pull_request_list;
mod review_panel;
mod review_panel_settings;
mod review_provider;
mod review_view;

pub use review_panel::ReviewPanel;

use gpui::App;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        review_panel::register(workspace);
    })
    .detach();
}
