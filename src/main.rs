use std::borrow::Cow;
use std::path::PathBuf;
use std::rc::Rc;

use chatgpt_rust_client::APP_NAME;
use chatgpt_rust_client::store::Store;
use gpui_kit::component::{Theme, ThemeConfig, ThemeRegistry, input};
use gpui_kit::*;

mod ui;

const THEME: &str = include_str!("../assets/patina-theme.json");

gpui_kit::assets::icon_assets!(ExtraIcons, [Paperclip, SquarePen, FileImage]);

/// The kit's component icons plus the few extra Lucide icons Patina draws.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}

/// `PATINA_DATA_DIR` points the app at another folder, e.g. seeded test data.
fn data_dir() -> PathBuf {
    std::env::var_os("PATINA_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(APP_NAME)
        })
}

/// Registers the bundled Patina themes; the window keeps them in step with the system appearance.
fn load_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(THEME)
        .expect("bundled theme is valid");
    // The Patina themes set colors only. Without the kit's code colors for each mode, dark
    // mode would keep the light syntax palette and its navy tokens would vanish.
    let with_code_colors = |name: &str, kit: &Rc<ThemeConfig>| {
        registry.themes().get(name).map(|theme| {
            let mut theme = ThemeConfig::clone(theme);
            theme.highlight = theme.highlight.or_else(|| kit.highlight.clone());
            Rc::new(theme)
        })
    };
    let light = with_code_colors("Patina Light", registry.default_light_theme());
    let dark = with_code_colors("Patina Dark", registry.default_dark_theme());
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
    Theme::change(cx.window_appearance(), None, cx);
}

fn with_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        let _ = window.update(cx, |_, window, _| f(window));
    }
}

/// Keys are bound before the menus are built so the menus show their shortcuts.
fn bind_keys_and_menus(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", ui::NewChat, None),
        KeyBinding::new("escape", ui::Stop, None),
        KeyBinding::new("cmd-,", ui::OpenSettings, None),
        KeyBinding::new("cmd-q", ui::Quit, None),
        KeyBinding::new("cmd-m", ui::Minimize, None),
    ]);
    cx.on_action(|_: &ui::Quit, cx| cx.quit());
    cx.on_action(|_: &ui::Minimize, cx| with_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &ui::Zoom, cx| with_window(cx, |window| window.zoom_window()));

    cx.set_menus([
        Menu::new(APP_NAME).items([
            MenuItem::action("About Patina", ui::About),
            MenuItem::separator(),
            MenuItem::action("Settings…", ui::OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit Patina", ui::Quit),
        ]),
        Menu::new("File").items([MenuItem::action("New Chat", ui::NewChat)]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", ui::Minimize),
            MenuItem::action("Zoom", ui::Zoom),
        ]),
    ]);
}

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(APP_NAME.into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(16.), px(16.))),
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1100.), px(760.)),
            cx,
        ))),
        window_min_size: Some(size(px(720.), px(480.))),
        ..Default::default()
    }
}

fn main() {
    let data_dir = data_dir();
    let store = match Store::open(data_dir.clone()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{APP_NAME} can't open {}: {e}", data_dir.display());
            std::process::exit(1);
        }
    };

    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        load_theme(cx);
        bind_keys_and_menus(cx);
        // One window: closing it quits, so the app never lingers without a way back in.
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let options = window_options(cx);
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| ui::Patina::new(store, &data_dir, window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::ThemeSet;

    #[test]
    fn bundled_theme_has_both_modes() {
        let set: ThemeSet = serde_json::from_str(super::THEME).unwrap();
        let names: Vec<_> = set.themes.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["Patina Light", "Patina Dark"]);
    }
}
