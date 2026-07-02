use gpui::Pixels;
use settings::{RegisterSetting, Settings};
use ui::px;
use workspace::dock::DockPosition;

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct ReviewPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
}

impl Settings for ReviewPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let review_panel = content.review_panel.clone().unwrap();
        Self {
            button: review_panel.button.unwrap(),
            dock: review_panel.dock.unwrap().into(),
            default_width: px(review_panel.default_width.unwrap()),
        }
    }
}
