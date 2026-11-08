use chatgpt_rust_client::APP_NAME;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::Patina;

const PLAN_USAGE_OFF: &str = "Plan usage wasn't enabled for this app. Sign in again and allow it.";

impl Patina {
    pub(super) fn render_sign_in(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let waiting = self.sign_in.cancel.is_some();
        let notice = if self.account.as_ref().is_some_and(|a| !a.plan_usage) {
            Some(PLAN_USAGE_OFF.into())
        } else {
            self.sign_in.notice.clone()
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px_8()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(rems(3.)).font_semibold().child(APP_NAME))
                    .child(
                        div()
                            .text_color(muted)
                            .child("A native chat app for your ChatGPT plan."),
                    ),
            )
            .when_some(notice, |this, notice| {
                this.child(
                    div()
                        .mt_8()
                        .max_w(rems(24.))
                        .text_sm()
                        .text_center()
                        .child(notice),
                )
            })
            .child(
                Button::new("sign-in")
                    .primary()
                    .large()
                    .mt_10()
                    .label(if waiting {
                        "Waiting for browser…"
                    } else {
                        "Continue with ChatGPT"
                    })
                    .disabled(waiting)
                    .on_click(cx.listener(|this, _, window, cx| this.start_sign_in(window, cx))),
            )
            // One fixed slot for Cancel or the error line, so the stack never jumps.
            .child(h_flex().h_8().mt_1().map(|this| {
                if waiting {
                    this.child(
                        Button::new("cancel-sign-in")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx))),
                    )
                } else if let Some(error) = self.sign_in.error.clone() {
                    this.max_w(rems(28.))
                        .text_sm()
                        .text_color(danger)
                        .child(div().truncate().child(error))
                } else {
                    this
                }
            }))
            .child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(muted)
                    .child("Opens your browser. Chats are stored only on this Mac."),
            )
    }
}
