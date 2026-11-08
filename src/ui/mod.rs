mod app;
mod chat;
mod composer;
mod settings;
mod sidebar;
mod sign_in;

pub use app::Patina;
use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::*;

gpui_kit::actions!(
    patina,
    [NewChat, Stop, OpenSettings, About, Quit, Minimize, Zoom]
);

pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// The conversation column: messages and the composer share one width.
const COLUMN: Rems = rems(47.5);

/// A title bar strip, drawn by Patina under the transparent native one: dragging it moves
/// the window and a double-click zooms or minimizes it, per the system setting.
fn title_band(id: &'static str, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    // The move starts on the first drag after a press, so a click stays a click.
    let pressed = window.use_keyed_state(id, cx, |_, _| false);
    div()
        .id(id)
        .h_12()
        .flex_none()
        .window_control_area(WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.titlebar_double_click())
        .on_mouse_down(
            MouseButton::Left,
            window.listener_for(&pressed, |pressed, _, _, _| *pressed = true),
        )
        .on_mouse_up(
            MouseButton::Left,
            window.listener_for(&pressed, |pressed, _, _, _| *pressed = false),
        )
        .on_mouse_down_out(window.listener_for(&pressed, |pressed, _, _, _| *pressed = false))
        .on_mouse_move(window.listener_for(&pressed, |pressed, _, window, _| {
            if std::mem::take(pressed) {
                window.start_window_move();
            }
        }))
}
