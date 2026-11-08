use std::fs;
use std::path::PathBuf;

use chatgpt_rust_client::store::{self, Store};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Backspace, Textarea};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::select::Select;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::chat::attachment_chip;
use super::{COLUMN, Patina, USAGE_URL};

const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Images the Responses API accepts, plus PDF.
fn mime_for(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// Something to attach: a picked or pasted file, or a pasted image.
enum Pick {
    File(PathBuf),
    Image(Image),
}

impl Pick {
    /// Reads and stores the bytes; the error is a sentence for the user.
    fn store(self, store: &Store) -> Result<store::Attachment, String> {
        let (name, mime, bytes) = match self {
            Pick::File(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mime =
                    mime_for(&name).ok_or_else(|| format!("“{name}” isn't an image or a PDF."))?;
                let len = fs::metadata(&path)
                    .map_err(|e| format!("Couldn't read “{name}”: {e}"))?
                    .len();
                if len > MAX_ATTACHMENT_BYTES {
                    return Err(format!("“{name}” is larger than 20 MB."));
                }
                let bytes = fs::read(&path).map_err(|e| format!("Couldn't read “{name}”: {e}"))?;
                (name, mime, bytes)
            }
            Pick::Image(image) => {
                let name = format!("Pasted image.{}", image.format.extension());
                if image.bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
                    return Err(format!("“{name}” is larger than 20 MB."));
                }
                (name, image.format.mime_type(), image.bytes)
            }
        };
        store
            .put_attachment(&name, mime, &bytes)
            .map_err(|e| format!("Couldn't attach “{name}”: {e}"))
    }
}

impl Patina {
    pub(super) fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update_in(cx, |this, window, cx| this.attach_paths(paths, window, cx));
            }
        })
        .detach();
    }

    pub(super) fn attach_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach(paths.into_iter().map(Pick::File).collect(), window, cx);
    }

    fn remove_pending(&mut self, sha256: &str, cx: &mut Context<Self>) {
        self.pending.retain(|p| p.sha256 != sha256);
        cx.notify();
    }

    /// Takes over a paste that carries files or an image; plain text falls through to the input.
    fn paste(&mut self, item: &ClipboardItem, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut picks = Vec::new();
        let mut has_text = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::ExternalPaths(paths) => {
                    picks.extend(paths.paths().iter().cloned().map(Pick::File))
                }
                ClipboardEntry::Image(image)
                    if matches!(
                        image.format,
                        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp | ImageFormat::Gif
                    ) =>
                {
                    picks.push(Pick::Image(image.clone()))
                }
                ClipboardEntry::String(_) => has_text = true,
                _ => {}
            }
        }
        let files = picks.iter().any(|p| matches!(p, Pick::File(_)));
        if picks.is_empty() || (has_text && !files) {
            return false;
        }
        self.attach(picks, window, cx);
        true
    }

    /// Stores attachments off the UI thread, in order with other writes, then adds them to
    /// the draft.
    fn attach(&mut self, picks: Vec<Pick>, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let job = self.disk.run(move || {
            picks
                .into_iter()
                .map(|pick| pick.store(&store))
                .collect::<Vec<_>>()
        });
        // ponytail: a chip removed before sending leaves its file in attachments/ until a chat
        // using the same bytes is deleted; prune unreferenced files if that ever adds up.
        cx.spawn_in(window, async move |this, cx| {
            let Some(results) = job.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                for result in results {
                    match result {
                        Ok(a) if this.pending.iter().all(|p| p.sha256 != a.sha256) => {
                            this.pending.push(a)
                        }
                        Ok(_) => {}
                        Err(message) => window.push_notification(message, cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_composer(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, surface, link) = (
            theme.muted_foreground,
            theme.input,
            theme.background,
            theme.link,
        );
        let (primary, on_primary) = (theme.primary, theme.primary_foreground);
        // One inset all round, and the corners concentric with the send button's.
        let inset = px(8.);
        let radius = theme.radius + inset;
        let streaming = self.reply.is_some();
        let can_send = self.can_send(cx);
        let web_search = self.settings.web_search;
        let loading_models = self.models.is_empty() && self.models_task.is_some();
        let view = cx.entity();
        let paste_view = view.downgrade();

        let cards: Vec<_> = self
            .pending
            .iter()
            .map(|a| {
                let sha = a.sha256.clone();
                let (remove_sha, menu_sha, menu_view) = (sha.clone(), sha.clone(), view.clone());
                div()
                    .id(SharedString::from(format!("pending-{sha}")))
                    .child(
                        attachment_chip(SharedString::from(format!("card-{sha}")), a, cx)
                            .on_remove(cx.listener(move |this, _, _, cx| {
                                this.remove_pending(&remove_sha, cx)
                            })),
                    )
                    .context_menu(move |menu, _, _| {
                        let (view, sha) = (menu_view.clone(), menu_sha.clone());
                        menu.item(PopupMenuItem::new("Remove").on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| this.remove_pending(&sha, cx))
                        }))
                    })
            })
            .collect();

        v_flex()
            .w_full()
            .max_w(COLUMN)
            .gap_2()
            .child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p(inset)
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(surface)
                    // The text field passes Backspace on when it has nothing to delete, so an
                    // empty draft gives up its last attachment, as in a token field.
                    .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                        if this.input.read(cx).value().is_empty() && this.pending.pop().is_some() {
                            cx.notify();
                        }
                    }))
                    .when(!cards.is_empty(), |this| {
                        // The cards leave room above for their remove control; this takes
                        // it back so the composer's inset stays even.
                        this.child(h_flex().flex_wrap().gap_2().mt(px(-6.)).children(cards))
                    })
                    .child(Textarea::new(&self.input).appearance(false).on_paste(
                        move |item, window, cx| {
                            paste_view
                                .update(cx, |this, cx| this.paste(item, window, cx))
                                .unwrap_or(false)
                        },
                    ))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .icon(Icon::new(Lucide::Paperclip))
                                    .accessibility_label("Attach files")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_files(window, cx)
                                    })),
                            )
                            .child(
                                // On, it says so in words and color, which hover can't mimic.
                                Button::new("web-search")
                                    .ghost()
                                    .icon(IconName::Globe)
                                    .selected(web_search)
                                    .toggled(web_search)
                                    .when(web_search, |this| this.label("Search").text_color(link))
                                    .accessibility_label("Search the web")
                                    .tooltip("Search the web")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_web_search(cx)),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                div().flex_none().child(
                                    Select::new(&self.model_select)
                                        .small()
                                        .appearance(false)
                                        .placeholder(if loading_models {
                                            "Loading models…"
                                        } else {
                                            "Model"
                                        })
                                        .disabled(loading_models)
                                        .accessibility_label("Model")
                                        .menu_width(rems(12.)),
                                ),
                            )
                            .child(if streaming {
                                Button::new("stop")
                                    .primary()
                                    .icon(IconName::Square)
                                    .accessibility_label("Stop")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.finish_reply(window, cx)
                                    }))
                            } else {
                                Button::new("send")
                                    .primary()
                                    .icon(IconName::ArrowUp)
                                    .accessibility_label("Send")
                                    .disabled(!can_send)
                                    // Dimmed rather than greyed out, so the main action stays
                                    // easy to find while the draft is empty.
                                    .when(!can_send, |this| {
                                        this.bg(primary).text_color(on_primary).opacity(0.5)
                                    })
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.send(window, cx)),
                                    )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .justify_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child("Using ChatGPT plan ·")
                    .child(
                        Link::new("manage-usage")
                            .href(USAGE_URL)
                            .child("Manage usage"),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::mime_for;

    #[test]
    fn only_images_and_pdfs_are_accepted() {
        assert_eq!(mime_for("photo.JPG"), Some("image/jpeg"));
        assert_eq!(mime_for("scan.pdf"), Some("application/pdf"));
        assert_eq!(mime_for("anim.webp"), Some("image/webp"));
        assert_eq!(mime_for("notes.txt"), None);
        assert_eq!(mime_for("README"), None);
    }
}
