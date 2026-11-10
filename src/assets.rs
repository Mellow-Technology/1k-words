//! Assets compiled into the binary.

use std::borrow::Cow;

use anyhow::Result;
use gpui_kit::{App, AssetSource, SharedString};

pub const BRACKET_ICON: &str = "icons/bracket.svg";
pub const BURST_ICON: &str = "icons/burst.svg";
pub const LIVE_ICON: &str = "icons/live.svg";

const ASSETS: &[(&str, &[u8])] = &[
    (BRACKET_ICON, include_bytes!("../assets/icons/bracket.svg")),
    (BURST_ICON, include_bytes!("../assets/icons/burst.svg")),
    (LIVE_ICON, include_bytes!("../assets/icons/live.svg")),
];

/// The UI font (Lato 2.0, SIL Open Font License; see `assets/fonts/OFL.txt`),
/// in the weights GPUI Kit's components use. Themes select it by family name.
pub const FONT_FAMILY: &str = "Lato";

const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Lato-Regular.ttf"),
    include_bytes!("../assets/fonts/Lato-Italic.ttf"),
    include_bytes!("../assets/fonts/Lato-Medium.ttf"),
    include_bytes!("../assets/fonts/Lato-Semibold.ttf"),
    include_bytes!("../assets/fonts/Lato-Bold.ttf"),
];

/// Registers the bundled fonts with GPUI's text system.
pub fn load_fonts(cx: &App) {
    let fonts = FONTS.iter().map(|font| Cow::Borrowed(*font)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("could not load fonts: {error:#}");
    } else if !cx
        .text_system()
        .all_font_names()
        .iter()
        .any(|name| name == FONT_FAMILY)
    {
        eprintln!("bundled fonts loaded, but {FONT_FAMILY} isn't available");
    }
}

// Lucide icons beyond GPUI Kit's default set.
gpui_kit::assets::icon_assets!(
    LucideIcons,
    [
        Album,
        CloudOff,
        CloudUpload,
        Eject,
        FolderPlus,
        Grid2x2,
        HardDrive,
        Images,
        Import,
        LayoutDashboard,
        Pin,
        ScanEye
    ]
);

/// The app's own assets, falling back to GPUI Kit's (e.g. component icons).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ASSETS.iter().find(|(asset_path, _)| *asset_path == path) {
            Some((_, data)) => Ok(Some(Cow::Borrowed(*data))),
            None => match LucideIcons.load(path)? {
                Some(data) => Ok(Some(data)),
                None => gpui_kit::assets::Assets.load(path),
            },
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets: Vec<SharedString> = ASSETS
            .iter()
            .filter(|(asset_path, _)| asset_path.starts_with(path))
            .map(|(asset_path, _)| (*asset_path).into())
            .collect();
        assets.extend(LucideIcons.list(path)?);
        assets.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(assets)
    }
}
