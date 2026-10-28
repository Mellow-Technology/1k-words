//! Acknowledgements in Settings: the licences of what's built into the app
//! that must be shipped with it, such as the UI font's.

use gpui_kit::component::setting::{SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::{IntoElement, ParentElement, Styled, div};

/// Lato's licence, which the SIL OFL requires to go with the font.
pub const FONT_LICENSE: &str = include_str!("../assets/fonts/OFL.txt");

pub fn page() -> SettingPage {
    SettingPage::new("Acknowledgements")
        .icon(IconName::Info)
        .resettable(false)
        .description("1k Words is built with the work of others, under these licences.")
        .group(
            SettingGroup::new()
                .title("Lato")
                .description(
                    "The interface font, by Łukasz Dziedzic, under the SIL Open Font License 1.1.",
                )
                .item(
                    SettingItem::render(|_, _, cx| {
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(FONT_LICENSE)
                            .into_any_element()
                    })
                    .keywords(["licence", "license", "font", "OFL"]),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_license_is_the_ofl() {
        assert!(FONT_LICENSE.contains("SIL OPEN FONT LICENSE Version 1.1"));
    }
}
