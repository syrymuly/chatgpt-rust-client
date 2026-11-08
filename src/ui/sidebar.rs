use chatgpt_rust_client::store::ChatSummary;
use chrono::{DateTime, Local, TimeZone as _};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Patina, USAGE_URL};

/// Which sidebar heading a chat last updated at `updated_at` (unix seconds) falls under.
pub(super) fn day_group(updated_at: i64, now: DateTime<Local>) -> &'static str {
    let Some(then) = Local.timestamp_opt(updated_at, 0).single() else {
        return "Older";
    };
    match (now.date_naive() - then.date_naive()).num_days() {
        ..=0 => "Today",
        1 => "Yesterday",
        2..=7 => "Previous 7 days",
        _ => "Older",
    }
}

/// A sidebar row: the active one is filled, the others fill on hover.
fn row(id: impl Into<ElementId>, active: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let (active_bg, hover_bg) = (theme.list_active, theme.list_hover);
    h_flex()
        .id(id)
        .h_8()
        .px_2()
        .gap_2()
        .rounded(theme.radius)
        .text_sm()
        .map(|this| {
            if active {
                this.bg(active_bg)
            } else {
                this.hover(move |style| style.bg(hover_bg))
            }
        })
}

/// The commands for one chat, shared by its right-click menu and its "…" button.
fn chat_menu(menu: PopupMenu, view: &Entity<Patina>, chat: &ChatSummary) -> PopupMenu {
    let (view, id, title) = (view.clone(), chat.id.clone(), chat.title.clone());
    menu.item(
        PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
            view.update(cx, |this, cx| {
                this.confirm_delete(id.clone(), title.clone(), window, cx)
            })
        }),
    )
}

fn account_menu(menu: PopupMenu, view: &Entity<Patina>) -> PopupMenu {
    let (settings_view, sign_out_view) = (view.clone(), view.clone());
    menu.item(
        PopupMenuItem::new("Settings…").on_click(move |_, window, cx| {
            settings_view.update(cx, |this, cx| this.open_settings(window, cx))
        }),
    )
    .item(PopupMenuItem::link("Manage usage", USAGE_URL))
    .separator()
    .item(
        PopupMenuItem::new("Sign out").on_click(move |_, window, cx| {
            sign_out_view.update(cx, |this, cx| this.sign_out(window, cx))
        }),
    )
}

impl Patina {
    pub(super) fn render_sidebar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (surface, edge, ink) = (
            theme.sidebar,
            theme.sidebar_border,
            theme.sidebar_foreground,
        );
        let view = cx.entity();
        let now = Local::now();

        let mut rows = Vec::with_capacity(self.chats.len() + 4);
        let mut heading = None;
        for chat in &self.chats {
            let label = day_group(chat.updated_at, now);
            if heading != Some(label) {
                rows.push(
                    div()
                        .px_2()
                        .when(heading.is_some(), |this| this.pt_4())
                        .pb_1()
                        .text_xs()
                        .font_medium()
                        .text_color(muted)
                        .child(label)
                        .into_any_element(),
                );
                heading = Some(label);
            }
            rows.push(self.render_chat_row(chat, &view, cx));
        }

        v_flex()
            .w(rems(16.25))
            .flex_none()
            .h_full()
            .bg(surface)
            .border_r_1()
            .border_color(edge)
            .text_color(ink)
            // Room for the traffic lights; the window drags from here.
            .child(div().h_12().flex_none())
            .child(
                div().px_2().pb_3().child(
                    row("new-chat", false, cx)
                        .child(Icon::new(Lucide::SquarePen).small())
                        .child("New chat")
                        .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                ),
            )
            .child(
                v_flex()
                    .id("chats")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    // ponytail: every row renders; move to a virtual list past a few thousand chats.
                    .children(rows),
            )
            .child(
                div().flex_none().px_2().py_2().child(
                    Button::new("account")
                        .ghost()
                        .w_full()
                        // Same inset as the rows above, so the email shares their text edge.
                        .px_2()
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(muted)
                                .children(self.account.as_ref().map(|a| a.email.clone())),
                        )
                        .accessibility_label("Account")
                        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                            account_menu(menu, &view)
                        }),
                ),
            )
    }

    fn render_chat_row(
        &self,
        chat: &ChatSummary,
        view: &Entity<Patina>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = chat.id == self.chat.id;
        let id = chat.id.clone();
        let (menu_view, menu_chat) = (view.clone(), chat.clone());
        let (button_view, button_chat) = (view.clone(), chat.clone());

        row(SharedString::from(format!("chat-{}", chat.id)), active, cx)
            .group("chat-row")
            .pr_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(chat.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .when(!active, |this| {
                        this.invisible()
                            .group_hover("chat-row", |style| style.visible())
                    })
                    // Keeps the row from opening the chat when the button is pressed.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(SharedString::from(format!("chat-menu-{}", chat.id)))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Ellipsis)
                            .accessibility_label("Chat actions")
                            .dropdown_menu(move |menu, _, _| {
                                chat_menu(menu, &button_view, &button_chat)
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.open_chat(&id, window, cx)))
            .context_menu(move |menu, _, _| chat_menu(menu, &menu_view, &menu_chat))
            .into_any_element()
    }

    pub(super) fn confirm_delete(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, id) = (view.clone(), id.clone());
            alert
                .title(SharedString::from(format!("Delete “{title}”?")))
                .description("This can't be undone.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .cancel_variant(ButtonVariant::Ghost)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.delete_chat(&id, window, cx));
                    true
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone as _};

    use super::day_group;

    #[test]
    fn chats_group_by_local_day() {
        let now = Local.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap();
        // `earliest`: 01:00 on Nov 1 2026 happens twice where US daylight time ends.
        let at = |d, h| {
            Local
                .with_ymd_and_hms(2026, 11, d, h, 0, 0)
                .earliest()
                .unwrap()
                .timestamp()
        };
        assert_eq!(day_group(at(8, 0), now), "Today");
        assert_eq!(day_group(at(8, 23), now), "Today");
        assert_eq!(day_group(at(7, 23), now), "Yesterday");
        assert_eq!(day_group(at(6, 1), now), "Previous 7 days");
        assert_eq!(day_group(at(1, 1), now), "Previous 7 days");
        assert_eq!(day_group(at(1, 1) - 86_400, now), "Older");
    }
}
