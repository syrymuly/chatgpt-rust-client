use chatgpt_rust_client::store::{self, Message, Role};
use chrono::Timelike as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::base::SelectableText;
use gpui_kit::component::attachment::{
    Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentTitle,
};
use gpui_kit::component::bubble::{Bubble, BubbleContent, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::message::{self, MessageAlignment, MessageContent};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::app::{Retry, TurnError};
use super::{COLUMN, Patina, title_band};

/// "Good evening, Sardar": the time of day plus the first name, when the account has one.
pub(super) fn greeting(name: Option<&str>, hour: u32) -> String {
    let part = match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    };
    match name.and_then(|n| n.split_whitespace().next()) {
        Some(first) => format!("{part}, {first}"),
        None => part.into(),
    }
}

/// A file card: type icon, name, and the format ("PDF", "PNG").
pub(super) fn attachment_chip(
    id: impl Into<ElementId>,
    attachment: &store::Attachment,
    cx: &App,
) -> Attachment {
    let icon = if attachment.mime.starts_with("image/") {
        Icon::new(Lucide::FileImage)
    } else {
        Icon::new(IconName::FileText)
    };
    let kind = attachment
        .mime
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_uppercase();
    Attachment::new()
        .id(id)
        .xsmall()
        // Long names are cut on the card.
        .tooltip(attachment.name.clone())
        .media(
            AttachmentMedia::new()
                .bg(cx.theme().transparent)
                .text_color(cx.theme().muted_foreground)
                .child(icon),
        )
        .content(
            AttachmentContent::new()
                .title(AttachmentTitle::new(attachment.name.clone()))
                .description(AttachmentDescription::new(kind)),
        )
}

impl Patina {
    pub(super) fn render_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let empty = self.chat.messages.is_empty();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        // Once replies scroll under the band, an edge shows where they are cut off.
        let top = self.scroller.read(cx).logical_scroll_top();
        let scrolled = !empty && (top.item_ix > 0 || top.offset_in_item > px(0.));
        let edge = if scrolled {
            theme.border
        } else {
            theme.transparent
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                title_band("chat-title", window, cx)
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_24()
                    .border_b_1()
                    .border_color(edge)
                    .when(!empty, |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_medium()
                                .text_color(muted)
                                .child(self.chat.title.clone()),
                        )
                    }),
            )
            .map(|this| {
                if empty {
                    this.child(self.render_empty(window, cx))
                } else {
                    this.child(self.render_messages(cx)).child(
                        h_flex()
                            .w_full()
                            .flex_none()
                            .justify_center()
                            .px_6()
                            .pt_2()
                            .pb_3()
                            .child(self.render_composer(window, cx)),
                    )
                }
            })
    }

    fn render_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.account.as_ref().and_then(|a| a.name.as_deref());
        let hello = greeting(name, chrono::Local::now().hour());
        let view = cx.entity();
        let danger = cx.theme().danger;
        // The greeting ends at the midline and the composer starts there, so attachments
        // and extra lines grow the composer downward and nothing above it moves.
        v_flex()
            .flex_1()
            .min_h_0()
            .px_6()
            // More room below than the title band takes above, so the resting group (greeting,
            // composer, plan line) sits at the window's center.
            .pb(rems(8.5))
            .child(
                v_flex()
                    .flex_1()
                    .justify_end()
                    .items_center()
                    .pb_8()
                    .child(div().text_size(rems(1.75)).child(hello)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .gap_3()
                    .child(self.render_composer(window, cx))
                    .when_some(self.error.clone(), |this, error| {
                        this.child(turn_note(&view, error, danger, window))
                    }),
            )
    }

    fn render_messages(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        div().flex_1().min_h_0().child(
            MessageScroller::new("messages", self.scroller.clone(), move |ix, window, cx| {
                render_row(&view, ix, window, cx)
            })
            .with_list_style(StyleRefinement::default().px_6().pt_2().pb_2())
            // The hover actions under a reply already add space, so rows sit closer than the default.
            .with_row_style(StyleRefinement::default().pb_6()),
        )
    }
}

fn render_row(view: &Entity<Patina>, ix: usize, window: &mut Window, cx: &mut App) -> AnyElement {
    let this = view.read(cx);
    let Some(message) = this.chat.messages.get(ix).cloned() else {
        return div().into_any_element();
    };
    let last = ix + 1 == this.chat.messages.len();
    let reply = this
        .reply
        .as_ref()
        .filter(|_| last)
        .map(|r| (r.view.clone(), r.searching));
    let theme = cx.theme();
    let (danger, muted) = (theme.danger, theme.muted_foreground);
    let note = match this.error.clone().filter(|_| last) {
        Some(error) => Some((error, danger)),
        // Stopped before the first word, or quit mid-reply: the turn can still be asked again.
        None if last && message.role == Role::User && this.reply.is_none() => Some((
            TurnError {
                message: "No reply.".into(),
                retry: Retry::Reply,
            },
            muted,
        )),
        None => None,
    };

    let body = match message.role {
        Role::User => user_message(ix, message, cx).into_any_element(),
        Role::Assistant => {
            assistant_message(view, ix, message, reply, last, window, cx).into_any_element()
        }
    };
    h_flex()
        .w_full()
        .justify_center()
        .child(
            v_flex()
                .w_full()
                .max_w(COLUMN)
                .gap_3()
                .child(body)
                .when_some(note, |this, (note, color)| {
                    this.child(turn_note(view, note, color, window))
                }),
        )
        .into_any_element()
}

fn user_message(ix: usize, message: Message, cx: &App) -> impl IntoElement {
    let chips = h_flex().flex_wrap().justify_end().gap_2().children(
        message
            .attachments
            .iter()
            .map(|a| attachment_chip(SharedString::from(format!("{ix}-{}", a.sha256)), a, cx)),
    );
    let content = MessageContent::new()
        .when(!message.attachments.is_empty(), |this| this.child(chips))
        .when(!message.text.is_empty(), |this| {
            this.bubble(
                Bubble::new()
                    .with_variant(BubbleVariant::Muted)
                    .content(BubbleContent::new().text_base().px_4())
                    .child(SelectableText::new(("user-text", ix), message.text)),
            )
        });
    message::Message::new()
        .alignment(MessageAlignment::End)
        .content(content)
}

fn assistant_message(
    view: &Entity<Patina>,
    ix: usize,
    message: Message,
    reply: Option<(Entity<TextViewState>, bool)>,
    last: bool,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let streaming = reply.is_some();
    let body = match reply {
        Some((_, searching)) if message.text.is_empty() => ShimmerText::new(if searching {
            "Searching the web"
        } else {
            "Thinking"
        })
        .text_color(cx.theme().muted_foreground)
        .into_any_element(),
        Some((state, _)) => TextView::new(&state)
            .selectable(true)
            .stream_fade(false)
            .into_any_element(),
        None => TextView::markdown(("reply", ix), message.text.clone())
            .selectable(true)
            .stream_fade(false)
            .into_any_element(),
    };
    v_flex()
        .w_full()
        .gap_2()
        .group("reply")
        .child(body)
        .when(!streaming, |this| {
            this.child(
                h_flex()
                    .gap_1()
                    // Puts the first glyph, not its button's edge, on the reply's text edge:
                    // (24 px small button - 14 px icon) / 2.
                    .ml(px(-5.))
                    // Older replies show their actions on hover; the latest keeps them in view.
                    .when(!last, |this| {
                        this.invisible()
                            .group_hover("reply", |style| style.visible())
                    })
                    .child(Clipboard::new(("copy", ix)).small().value(message.text))
                    .when(last, |this| {
                        this.child(
                            Button::new("regenerate")
                                .ghost()
                                .small()
                                .icon(IconName::RefreshCw)
                                .tooltip("Regenerate")
                                .on_click(window.listener_for(view, |this, _, window, cx| {
                                    this.regenerate(window, cx)
                                })),
                        )
                    }),
            )
        })
}

/// A line under the last turn saying why it has no reply, with a way to ask again.
fn turn_note(
    view: &Entity<Patina>,
    note: TurnError,
    color: Hsla,
    window: &mut Window,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .text_sm()
        .child(div().min_w_0().text_color(color).child(note.message))
        .child(
            Button::new("retry")
                .ghost()
                .small()
                .icon(IconName::RefreshCw)
                .label("Retry")
                .on_click(window.listener_for(view, move |this, _, window, cx| {
                    this.retry(note.retry, window, cx)
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::greeting;

    #[test]
    fn greeting_uses_time_of_day_and_first_name() {
        assert_eq!(greeting(Some("Sardar Syrymuly"), 9), "Good morning, Sardar");
        assert_eq!(greeting(Some("Sardar"), 12), "Good afternoon, Sardar");
        assert_eq!(greeting(None, 20), "Good evening");
        assert_eq!(greeting(Some("  "), 2), "Good evening");
    }
}
