use chatgpt_rust_client::APP_NAME;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, FocusableExt as _, Sizable as _, StyledExt as _, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Patina, title_band};

const PLAN_USAGE_OFF: &str = "Plan usage wasn't enabled for this app. Sign in again and allow it.";

impl Patina {
    pub(super) fn render_sign_in(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let pressed = cx.theme().primary_active;
        let waiting = self.sign_in.cancel.is_some();
        let notice = if self.account.as_ref().is_some_and(|a| !a.plan_usage) {
            Some(PLAN_USAGE_OFF.into())
        } else {
            self.sign_in.notice.clone()
        };
        let has_notice = notice.is_some();

        v_flex()
            .relative()
            .size_full()
            .items_center()
            .justify_center()
            .px_8()
            // The title's line box leaves more air above the caps than the footnote leaves
            // below its text; this evens the visible stack out on the window's center.
            .pb_5()
            // Out of the flow, so the stack stays centered on the whole window.
            .child(
                title_band("sign-in-title", window, cx)
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0(),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(img("mark.svg").w(rems(3.5)).h(rems(3.75)).mb_3())
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
                        .mt_6()
                        .max_w(rems(30.))
                        .text_sm()
                        .text_center()
                        .child(notice),
                )
            })
            .child(
                Button::new("sign-in")
                    .primary()
                    .large()
                    // The button explains the notice above it, so it sits closer to it.
                    .map(|this| {
                        if has_notice {
                            this.mt_4()
                        } else {
                            this.mt_10()
                        }
                    })
                    // Both labels fit, so the stack doesn't shift when it changes.
                    .w(rems(13.5))
                    // Focused at launch so Return starts it; the ring only follows the keyboard.
                    .track_focus(&self.sign_in_button)
                    .focus_ring(window.last_input_was_keyboard())
                    .label(if waiting {
                        "Waiting for browser…"
                    } else {
                        "Continue with ChatGPT"
                    })
                    // Keeps the primary colors (the label is the status) and ignores clicks. The
                    // pressed fill offsets the kit's loading fade, keeping the label above 5:1.
                    .when(waiting, |this| {
                        this.icon(Spinner::new()).loading(true).bg(pressed)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.start_sign_in(window, cx))),
            )
            // One fixed two-line slot for Cancel, an error or a late notice, so the stack
            // never jumps and nothing is cut off.
            .child(
                v_flex()
                    .mt_2()
                    .h_12()
                    .max_w(rems(28.))
                    .items_center()
                    .text_sm()
                    .text_center()
                    .map(|this| {
                        if waiting {
                            this.child(
                                Button::new("cancel-sign-in")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_sign_in(cx)),
                                    ),
                            )
                        } else if let Some(error) = self.sign_in.error.clone() {
                            this.text_color(danger).child(error)
                        } else if let Some(late) = self.sign_in.late_notice.clone() {
                            this.text_color(muted).child(late)
                        } else {
                            this
                        }
                    }),
            )
            .child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(muted)
                    .child("Opens your browser. Chats are stored only on this Mac."),
            )
    }
}
