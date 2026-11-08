mod app;
mod sign_in;

pub use app::Patina;

gpui_kit::actions!(patina, [Quit, Minimize, Zoom]);
