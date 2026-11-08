use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::*;

const THEME: &str = include_str!("../assets/patina-theme.json");

struct App;

impl Render for App {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child("Hello")
    }
}

/// Registers the bundled Patina themes and follows the system appearance.
fn load_theme(cx: &mut gpui_kit::App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(THEME)
        .expect("bundled theme is valid");
    let light = registry.themes().get("Patina Light").cloned();
    let dark = registry.themes().get("Patina Dark").cloned();
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
    Theme::change(cx.window_appearance(), None, cx);
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            load_theme(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| App))
                .expect("failed to open window");
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
