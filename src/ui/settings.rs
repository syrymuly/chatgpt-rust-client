use chatgpt_rust_client::APP_NAME;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogDescription, DialogFooter};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::link::Link;
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use super::{Patina, USAGE_URL};

impl Patina {
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone().filter(|_| self.signed_in()) else {
            return;
        };
        let view = cx.entity();
        let instructions = self.instructions.clone();
        let email = SharedString::from(account.email);
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let view = view.clone();
            dialog.title("Settings").child(
                v_flex()
                    .gap_6()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().text_sm().font_medium().child("Custom instructions"))
                            .child(Textarea::new(&instructions))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Sent with every message."),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .justify_between()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_sm().font_medium().child("Account"))
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .text_sm()
                                            .text_color(muted)
                                            .child(div().min_w_0().truncate().child(email.clone()))
                                            .child("·")
                                            .child(
                                                Link::new("settings-usage")
                                                    .href(USAGE_URL)
                                                    .child("Manage usage"),
                                            ),
                                    ),
                            )
                            .child(Button::new("sign-out").label("Sign out").on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    view.update(cx, |this, cx| this.sign_out(window, cx));
                                },
                            )),
                    ),
            )
        });
    }

    /// Shown once, after the first sign-in that grants plan usage.
    pub(super) fn open_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.welcome_seen = true;
        self.save_settings();
        open_notice(
            window,
            cx,
            "You're using your ChatGPT plan",
            "Eligible usage in this app uses your ChatGPT plan. Manage usage in your ChatGPT settings.",
            "Got it",
            false,
        );
    }
}

pub(super) fn open_usage_limit(window: &mut Window, cx: &mut App) {
    open_notice(
        window,
        cx,
        "Usage limit reached",
        "Review your plan or this app's limit in ChatGPT settings.",
        "Manage usage",
        true,
    );
}

pub(super) fn open_about(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, _| {
        dialog.title(APP_NAME).child(
            DialogDescription::new()
                .child("A native chat app for your ChatGPT plan.")
                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
        )
    });
}

/// A short dialog with a single primary action; `opens_usage` sends that action to ChatGPT settings.
fn open_notice(
    window: &mut Window,
    cx: &mut App,
    title: &'static str,
    body: &'static str,
    action: &'static str,
    opens_usage: bool,
) {
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(title)
            .child(DialogDescription::new().child(body))
            .footer(
                DialogFooter::new().child(
                    Button::new("notice-action")
                        .primary()
                        .label(action)
                        .on_click(move |_, window, cx| {
                            if opens_usage {
                                cx.open_url(USAGE_URL);
                            }
                            window.close_dialog(cx);
                        }),
                ),
            )
    });
}
