use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chatgpt_rust_client::APP_NAME;
use chatgpt_rust_client::auth::{Account, Auth, AuthError};
use chatgpt_rust_client::store::{Settings, Store};
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The window's root view. Owns the account, local data and every flow that talks to the network.
pub struct Patina {
    pub(super) auth: Arc<Auth>,
    pub(super) store: Store,
    pub(super) settings: Settings,
    /// `Some` with `plan_usage == false` still shows the sign-in screen, with a notice.
    pub(super) account: Option<Account>,
    /// Debug builds only, see [`preview_account`].
    pub(super) preview: bool,
    pub(super) sign_in: SignIn,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
pub(super) struct SignIn {
    /// Set while the browser flow runs; storing `true` ends it.
    pub(super) cancel: Option<Arc<AtomicBool>>,
    pub(super) error: Option<SharedString>,
    pub(super) notice: Option<SharedString>,
    task: Option<Task<()>>,
}

/// `PATINA_PREVIEW=1` in a debug build skips sign-in with a fake account, so screens can be
/// checked without a ChatGPT login. Release builds never take this path.
fn preview_account() -> Option<Account> {
    (cfg!(debug_assertions) && std::env::var("PATINA_PREVIEW").as_deref() == Ok("1")).then(|| {
        Account {
            email: "you@example.com".into(),
            name: Some("Sardar".into()),
            plan_usage: true,
        }
    })
}

/// Runs blocking work (network, Keychain) on its own thread so the UI thread never waits on it.
pub(super) fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = Option<T>> {
    let (tx, rx) = smol::channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(work());
    });
    async move { rx.recv().await.ok() }
}

impl Patina {
    pub fn new(store: Store, data_dir: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let preview = preview_account();
        let auth = Arc::new(Auth::load(data_dir));
        let account = preview.clone().or_else(|| auth.account());
        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx)
        });
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            auth,
            settings: store.settings(),
            store,
            account,
            preview: preview.is_some(),
            sign_in: SignIn::default(),
            focus_handle,
            _subscriptions: vec![appearance],
        }
    }

    pub(super) fn signed_in(&self) -> bool {
        self.account.as_ref().is_some_and(|a| a.plan_usage)
    }

    pub(super) fn start_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (url_tx, url_rx) = smol::channel::bounded::<String>(1);
        let job = blocking({
            let auth = self.auth.clone();
            let cancel = cancel.clone();
            move || {
                auth.sign_in(APP_NAME, &cancel, |url| {
                    let _ = url_tx.send_blocking(url.into());
                })
            }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            // The sender is dropped when sign-in fails before it gets to open the browser.
            if let Ok(url) = url_rx.recv().await {
                let _ = cx.update(|_, cx| cx.open_url(&url));
            }
            if let Some(result) = job.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.finish_sign_in(result, window, cx)
                });
            }
        });
        self.sign_in = SignIn {
            cancel: Some(cancel),
            task: Some(task),
            ..SignIn::default()
        };
        cx.notify();
    }

    pub(super) fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.sign_in.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.sign_in.task = None;
        cx.notify();
    }

    fn finish_sign_in(
        &mut self,
        result: Result<Account, AuthError>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sign_in.cancel = None;
        self.sign_in.task = None;
        match result {
            Ok(account) => self.account = Some(account),
            Err(AuthError::Cancelled) => {}
            Err(e) => self.sign_in.error = Some(e.to_string().into()),
        }
        cx.notify();
    }
}

impl Render for Patina {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("patina")
            .key_context("Patina")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .map(|this| {
                if self.signed_in() {
                    this.child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("Signed in"),
                    )
                } else {
                    this.child(self.render_sign_in(cx))
                }
            })
    }
}
