mod app;
mod chat;
mod composer;
mod settings;
mod sidebar;
mod sign_in;

pub use app::Patina;
use gpui_kit::{Rems, rems};

gpui_kit::actions!(
    patina,
    [NewChat, Stop, OpenSettings, About, Quit, Minimize, Zoom]
);

pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// The conversation column: messages and the composer share one width.
const COLUMN: Rems = rems(47.5);
