use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chatgpt_rust_client::APP_NAME;
use chatgpt_rust_client::api::{self, ApiError, Model};
use chatgpt_rust_client::auth::{Account, Auth, AuthError};
use chatgpt_rust_client::store::{self, Chat, ChatSummary, Message, Role, Settings, Store};
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::select::{SelectEvent, SelectItem, SelectState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::{ActiveTheme as _, IndexPath, Theme, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::settings::{open_about, open_usage_limit};
use super::{About, DeleteChat, NewChat, NextChat, OpenSettings, PreviousChat, Stop};

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
    pub(super) models: Vec<Model>,
    /// Newest first, as the sidebar shows them.
    pub(super) chats: Vec<ChatSummary>,
    /// Not on disk until its first message is sent.
    pub(super) chat: Chat,
    pub(super) reply: Option<Reply>,
    /// Shown under the last turn of the open chat.
    pub(super) error: Option<TurnError>,
    /// Stored already (content-addressed), sent with the next message.
    pub(super) pending: Vec<store::Attachment>,
    pub(super) input: Entity<TextareaState>,
    pub(super) model_select: Entity<SelectState<Vec<ModelItem>>>,
    pub(super) scroller: Entity<MessageScrollerState>,
    /// The custom instructions field in Settings; edits are saved once typing pauses.
    pub(super) instructions: Entity<TextareaState>,
    pub(super) disk: Disk,
    focus_handle: FocusHandle,
    /// The sign-in screen's only action, focused so Return starts it.
    pub(super) sign_in_button: FocusHandle,
    /// `Some` while the model list is loading.
    pub(super) models_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    instructions_save: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
pub(super) struct SignIn {
    /// Set while the browser flow runs; storing `true` ends it.
    pub(super) cancel: Option<Arc<AtomicBool>>,
    pub(super) error: Option<SharedString>,
    /// Shown above the button; present when the screen appears.
    pub(super) notice: Option<SharedString>,
    /// Arrives while the screen is up (the revoke result), so it takes the slot under the
    /// button instead of pushing the stack around.
    pub(super) late_notice: Option<SharedString>,
    task: Option<Task<()>>,
}

/// The assistant reply being streamed into the last message.
pub(super) struct Reply {
    cancel: Arc<AtomicBool>,
    /// Parsed incrementally as deltas arrive, instead of re-parsing the whole reply.
    pub(super) view: Entity<TextViewState>,
    pub(super) searching: bool,
    _task: Task<()>,
}

#[derive(Clone)]
pub(super) struct TurnError {
    pub(super) message: SharedString,
    pub(super) retry: Retry,
}

#[derive(Clone, Copy)]
pub(super) enum Retry {
    Reply,
    Models,
}

#[derive(Clone)]
pub(super) struct ModelItem(Model);

impl SelectItem for ModelItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.0.display_name.clone().into()
    }

    fn value(&self) -> &String {
        &self.0.slug
    }
}

/// Why a network call failed, as far as the UI cares.
enum Failure {
    SignIn,
    UsageLimit,
    Other(String),
}

const OFFLINE: &str = "Couldn't reach ChatGPT. Check your connection and try again.";

/// The raw detail of a failure goes to the log; the user gets what happened and what to do.
fn log(detail: impl std::fmt::Display) {
    eprintln!("{APP_NAME}: {detail}");
}

/// What the sign-in screen says when the browser flow fails.
pub(super) fn sign_in_message(e: &AuthError) -> &'static str {
    log(e);
    match e {
        AuthError::Network(_) => OFFLINE,
        AuthError::Denied => "Access wasn't allowed in ChatGPT. Try again and allow it.",
        _ => "Sign-in didn't finish. Try again.",
    }
}

impl From<AuthError> for Failure {
    fn from(e: AuthError) -> Self {
        if e == AuthError::SignInRequired {
            return Failure::SignIn;
        }
        log(&e);
        Failure::Other(match e {
            AuthError::Network(_) => OFFLINE.into(),
            _ => "Couldn't refresh your ChatGPT session. Try again.".into(),
        })
    }
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        if e.is_auth() {
            return Failure::SignIn;
        } else if e.is_usage_limit() {
            return Failure::UsageLimit;
        }
        log(&e);
        Failure::Other(match (e.status, e.code.as_deref()) {
            // No HTTP status and no code: the connection failed or dropped mid-reply.
            (None, None) => OFFLINE.into(),
            _ if e.is_transient() => {
                "ChatGPT is unavailable right now. Try again in a moment.".into()
            }
            _ => e.message,
        })
    }
}

enum ReplyMsg {
    Event(api::Event),
    NoWebSearch,
    Done(Result<(), Failure>),
}

/// Everything a reply needs, moved onto its own thread.
struct ReplyJob {
    auth: Arc<Auth>,
    store: Store,
    history: Vec<Message>,
    model: String,
    instructions: String,
    web_search: bool,
    cancel: Arc<AtomicBool>,
}

impl ReplyJob {
    fn run(self, tx: smol::channel::Sender<ReplyMsg>) {
        let result = self.stream(&tx);
        let _ = tx.send_blocking(ReplyMsg::Done(result));
    }

    fn stream(&self, tx: &smol::channel::Sender<ReplyMsg>) -> Result<(), Failure> {
        let token = self.auth.access_token()?;
        let turns: Vec<_> = self.history.iter().map(|m| self.turn(m)).collect();
        let send = |event| {
            let _ = tx.send_blocking(ReplyMsg::Event(event));
        };
        let mut request = api::Request {
            model: &self.model,
            instructions: Some(&self.instructions),
            turns: &turns,
            web_search: self.web_search,
        };
        match api::stream(&token, &request, &self.cancel, send) {
            // The model or account can't search: answer without it rather than fail.
            Err(e)
                if request.web_search
                    && e.code.as_deref() == Some("subscription_sharing_unsupported_capability")
                    && e.param.as_deref().is_some_and(|p| p.contains("tools")) =>
            {
                let _ = tx.send_blocking(ReplyMsg::NoWebSearch);
                request.web_search = false;
                api::stream(&token, &request, &self.cancel, send).map_err(Failure::from)
            }
            result => result.map_err(Failure::from),
        }
    }

    /// A file that can't be read any more is named in the text instead, so one lost
    /// attachment doesn't fail every later turn of the chat.
    fn turn(&self, message: &Message) -> api::Turn {
        let mut text = message.text.clone();
        let mut attachments = Vec::new();
        for a in &message.attachments {
            match self.store.attachment_bytes(a) {
                Ok(data) => attachments.push(api::Attachment {
                    name: a.name.clone(),
                    mime: a.mime.clone(),
                    data,
                }),
                Err(e) => {
                    log(format!("can't read attachment {}: {e}", a.name));
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("[attachment unavailable: {}]", a.name));
                }
            }
        }
        api::Turn {
            role: match message.role {
                Role::User => api::Role::User,
                Role::Assistant => api::Role::Assistant,
            },
            text,
            attachments,
        }
    }
}

/// Runs store writes, and the loads that must see them, in order on one thread: the UI
/// never waits on an fsync, and a chat is never read back older than its last save.
#[derive(Clone)]
pub(super) struct Disk(mpsc::Sender<Box<dyn FnOnce() + Send>>);

impl Disk {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::spawn(move || rx.into_iter().for_each(|job| job()));
        Disk(tx)
    }

    fn queue(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.0.send(Box::new(job));
    }

    /// Queues `work` behind every earlier job and resolves with its result.
    pub(super) fn run<T, F>(&self, work: F) -> impl Future<Output = Option<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = smol::channel::bounded(1);
        self.queue(move || {
            let _ = tx.send_blocking(work());
        });
        async move { rx.recv().await.ok() }
    }

    /// Blocks until every queued job is done; only for quitting.
    fn flush(&self) {
        smol::block_on(self.run(|| ()));
    }
}

/// Deletes a chat but keeps the files of draft attachments, which no saved chat
/// references yet and the store's cleanup would otherwise remove.
fn delete_keeping(store: &Store, id: &str, drafts: &[store::Attachment]) -> io::Result<()> {
    let kept: Vec<_> = drafts
        .iter()
        .filter_map(|a| Some((a, store.attachment_bytes(a).ok()?)))
        .collect();
    store.delete(id)?;
    for (a, bytes) in kept {
        // A no-op for files the cleanup left alone.
        let _ = store.put_attachment(&a.name, &a.mime, &bytes);
    }
    Ok(())
}

const SESSION_ENDED: &str = "Your session ended. Sign in again to continue.";
const REVOKE_UNCONFIRMED: &str =
    "Signed out. If this Mac was offline, also disconnect Patina in ChatGPT settings.";

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

fn preview_models() -> Vec<Model> {
    [
        ("gpt-5", "GPT-5"),
        ("gpt-5-thinking", "GPT-5 Thinking"),
        ("gpt-5-mini", "GPT-5 mini"),
    ]
    .map(|(slug, name)| Model {
        slug: slug.into(),
        display_name: name.into(),
    })
    .into()
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Patina {
    pub fn new(store: Store, data_dir: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let preview = preview_account();
        let auth = Arc::new(Auth::load(data_dir));
        let account = preview.clone().or_else(|| auth.account());
        let chats = store.list();
        let chat = chats
            .first()
            .and_then(|c| store.load(&c.id).ok())
            .unwrap_or_else(|| store.new_chat());

        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder("Ask anything")
        });
        let model_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        let scroller = cx.new(|cx| MessageScrollerState::new(chat.messages.len(), cx));
        let settings = store.settings();
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("For example: answer briefly, I work in Rust.")
                .default_value(settings.instructions.clone())
        });

        Theme::sync_system_appearance(Some(window), cx);
        let subscriptions =
            vec![
                cx.observe_window_appearance(window, |_, window, cx| {
                    Theme::sync_system_appearance(Some(window), cx)
                }),
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    }
                }),
                cx.subscribe(
                    &model_select,
                    |this, _, event: &SelectEvent<Vec<ModelItem>>, _| {
                        if let SelectEvent::Confirm(Some(slug)) = event {
                            this.settings.model = Some(slug.clone());
                            this.save_settings();
                        }
                    },
                ),
                // The jump-to-latest button follows the scroll position.
                cx.observe(&scroller, |_, _, cx| cx.notify()),
                cx.subscribe(&instructions, |this, input, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        this.settings.instructions = input.read(cx).value().to_string();
                        // Written once typing pauses, not on every keystroke.
                        this.instructions_save = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(500))
                                .await;
                            let _ = this.update(cx, |this, _| {
                                this.instructions_save = None;
                                this.save_settings();
                            });
                        }));
                    }
                }),
                cx.on_app_quit(|this, _| {
                    this.save_before_exit();
                    async {}
                }),
                // Closing the window drops this view before the app quits.
                cx.on_release(|this, _| this.save_before_exit()),
            ];

        let mut this = Self {
            auth,
            settings,
            store,
            account,
            preview: preview.is_some(),
            sign_in: SignIn::default(),
            models: Vec::new(),
            chats,
            chat,
            reply: None,
            error: None,
            pending: Vec::new(),
            input,
            model_select,
            scroller,
            instructions,
            disk: Disk::new(),
            focus_handle: cx.focus_handle(),
            sign_in_button: cx.focus_handle(),
            models_task: None,
            open_task: None,
            instructions_save: None,
            _subscriptions: subscriptions,
        };
        this.enter(window, cx);
        if this.preview {
            cx.defer_in(window, |this, window, cx| {
                this.run_preview_steps(window, cx)
            });
        }
        this
    }

    /// Debug preview only: `PATINA_PREVIEW_PROMPT` sends a message on launch,
    /// `PATINA_PREVIEW_DIALOG` (settings, welcome, usage, about, delete) opens a dialog,
    /// `PATINA_PREVIEW_ATTACH=a.png:b.pdf` attaches files, `PATINA_PREVIEW_SIGNIN` (waiting,
    /// error, planoff, ended, revoke) shows a sign-in state and `PATINA_PREVIEW_SIZE=720x480`
    /// resizes the window, so states that need input can be checked without a keyboard.
    fn run_preview_steps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preview_size = std::env::var("PATINA_PREVIEW_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some(size(px(w.parse().ok()?), px(h.parse().ok()?)))
        });
        if let Some(preview_size) = preview_size {
            window.resize(preview_size);
        }
        if let Some(paths) = std::env::var_os("PATINA_PREVIEW_ATTACH") {
            self.attach_paths(std::env::split_paths(&paths).collect(), window, cx);
        }
        if let Ok(state) = std::env::var("PATINA_PREVIEW_SIGNIN") {
            self.account = None;
            match state.as_str() {
                "waiting" => self.sign_in.cancel = Some(Arc::default()),
                "error" => {
                    let e = AuthError::Network("dns failed: failed to lookup address".into());
                    self.sign_in.error = Some(sign_in_message(&e).into());
                }
                "planoff" => {
                    self.account = preview_account().map(|a| Account {
                        plan_usage: false,
                        ..a
                    })
                }
                "ended" => self.sign_in.notice = Some(SESSION_ENDED.into()),
                "revoke" => self.sign_in.late_notice = Some(REVOKE_UNCONFIRMED.into()),
                _ => {}
            }
            self.enter(window, cx);
        }
        if let Ok(prompt) = std::env::var("PATINA_PREVIEW_PROMPT") {
            self.input
                .update(cx, |input, cx| input.set_value(prompt, window, cx));
            self.send(window, cx);
        }
        match std::env::var("PATINA_PREVIEW_DIALOG").as_deref() {
            Ok("settings") => self.open_settings(window, cx),
            Ok("welcome") => self.open_welcome(window, cx),
            Ok("usage") => open_usage_limit(window, cx),
            Ok("about") => open_about(window, cx),
            Ok("delete") => {
                let (id, title) = (self.chat.id.clone(), self.chat.title.clone());
                self.confirm_delete(id, title, window, cx)
            }
            _ => {}
        }
    }

    pub(super) fn signed_in(&self) -> bool {
        self.account.as_ref().is_some_and(|a| a.plan_usage)
    }

    /// Puts focus where the current screen expects it and loads what it needs.
    fn enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.signed_in() {
            self.load_models(window, cx);
            self.input.update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.sign_in_button.focus(window, cx);
        }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sign_in.cancel = None;
        self.sign_in.task = None;
        match result {
            Ok(account) => {
                self.account = Some(account);
                self.enter(window, cx);
                if self.signed_in() && !self.settings.welcome_seen {
                    self.open_welcome(window, cx);
                }
            }
            Err(AuthError::Cancelled) => {}
            Err(e) => self.sign_in.error = Some(sign_in_message(&e).into()),
        }
        cx.notify();
    }

    /// Stops requests, then revokes in the background; the UI goes back to sign-in at once.
    pub(super) fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_reply(window, cx);
        self.account = None;
        self.models.clear();
        self.models_task = None;
        self.sign_in = SignIn::default();
        self.enter(window, cx);
        cx.notify();
        if self.preview {
            return;
        }
        let job = blocking({
            let auth = self.auth.clone();
            move || auth.sign_out()
        });
        cx.spawn(async move |this, cx| {
            if job.await == Some(false) {
                let _ = this.update(cx, |this, cx| {
                    this.sign_in.late_notice = Some(REVOKE_UNCONFIRMED.into());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn session_ended(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sign_out(window, cx);
        self.sign_in.notice = Some(SESSION_ENDED.into());
    }

    pub(super) fn load_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return self.set_models(preview_models(), window, cx);
        }
        let job = blocking({
            let auth = self.auth.clone();
            move || -> Result<Vec<Model>, Failure> { Ok(api::list_models(&auth.access_token()?)?) }
        });
        self.models_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(result) = job.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.models_task = None;
                match result {
                    Ok(models) => this.set_models(models, window, cx),
                    Err(failure) => this.fail(failure, Retry::Models, window, cx),
                }
            });
        }));
        cx.notify();
    }

    /// Asks for the model list again when it is missing, e.g. after a failed load at
    /// launch whose Retry was cleared by switching chats.
    fn ensure_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.signed_in() && self.models.is_empty() && self.models_task.is_none() {
            self.load_models(window, cx);
        }
    }

    /// Keeps the saved model when the account still offers it, else picks the first.
    fn set_models(&mut self, models: Vec<Model>, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .settings
            .model
            .as_ref()
            .and_then(|slug| models.iter().position(|m| &m.slug == slug))
            .or((!models.is_empty()).then_some(0));
        let slug = selected.map(|ix| models[ix].slug.clone());
        if slug != self.settings.model {
            self.settings.model = slug;
            self.save_settings();
        }
        let items = models.iter().cloned().map(ModelItem).collect();
        self.model_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(selected.map(IndexPath::new), window, cx);
        });
        self.models = models;
        if matches!(
            self.error,
            Some(TurnError {
                retry: Retry::Models,
                ..
            })
        ) {
            self.error = None;
        }
        cx.notify();
    }

    pub(super) fn save_settings(&self) {
        let (store, settings) = (self.store.clone(), self.settings.clone());
        // ponytail: a failed settings write only loses a preference, so it is not surfaced.
        self.disk.queue(move || {
            let _ = store.save_settings(&settings);
        });
    }

    pub(super) fn can_send(&self, cx: &App) -> bool {
        self.reply.is_none()
            && self.settings.model.is_some()
            && (!self.pending.is_empty() || !self.input.read(cx).value().trim().is_empty())
    }

    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        let attachments = std::mem::take(&mut self.pending);
        if self.chat.messages.is_empty() {
            let first = match attachments.first() {
                Some(a) if text.is_empty() => &a.name,
                _ => &text,
            };
            self.chat.title = store::title_from(first);
        }
        self.chat.messages.push(Message {
            role: Role::User,
            text,
            attachments,
        });
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.scroller.update(cx, |rows, cx| rows.scroll_to_end(cx));
        self.start_reply(window, cx);
    }

    fn start_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_models(window, cx);
        let Some(model) = self.settings.model.clone() else {
            return;
        };
        self.error = None;
        // On disk before the reply starts, so a quit mid-reply never leaves an empty one.
        self.save_chat(window, cx);
        let job = ReplyJob {
            auth: self.auth.clone(),
            store: self.store.clone(),
            history: self.chat.messages.clone(),
            model,
            instructions: self.settings.instructions.clone(),
            web_search: self.settings.web_search,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let cancel = job.cancel.clone();
        let (tx, rx) = smol::channel::unbounded();
        std::thread::spawn(move || job.run(tx));
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.on_reply(msg, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.chat.messages.push(Message {
            role: Role::Assistant,
            text: String::new(),
            attachments: Vec::new(),
        });
        self.reply = Some(Reply {
            cancel,
            view: cx.new(|cx| TextViewState::markdown("", cx)),
            searching: false,
            _task: task,
        });
        self.sync_rows(cx);
        cx.notify();
    }

    fn on_reply(&mut self, msg: ReplyMsg, window: &mut Window, cx: &mut Context<Self>) {
        let Some(reply) = &mut self.reply else {
            return;
        };
        match msg {
            ReplyMsg::Event(api::Event::Text(delta)) => {
                reply.searching = false;
                reply.view.update(cx, |view, cx| view.push_str(&delta, cx));
                if let Some(last) = self.chat.messages.last_mut() {
                    last.text.push_str(&delta);
                }
            }
            ReplyMsg::Event(api::Event::Searching) => reply.searching = true,
            ReplyMsg::Event(api::Event::SearchDone) => reply.searching = false,
            ReplyMsg::NoWebSearch => {
                self.settings.web_search = false;
                self.save_settings();
                window.push_notification(
                    "Web search isn't available for this model, so it was turned off.",
                    cx,
                );
            }
            ReplyMsg::Done(result) => {
                self.finish_reply(window, cx);
                if let Err(failure) = result {
                    self.fail(failure, Retry::Reply, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Ends the reply in flight (if any), keeping whatever text already arrived.
    pub(super) fn finish_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(reply) = self.reply.take() else {
            return;
        };
        reply.cancel.store(true, Ordering::Relaxed);
        if self
            .chat
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Assistant && m.text.is_empty())
        {
            self.chat.messages.pop();
        }
        self.save_chat(window, cx);
        self.sync_rows(cx);
        cx.notify();
    }

    fn fail(
        &mut self,
        failure: Failure,
        retry: Retry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = match failure {
            Failure::SignIn if !self.preview => return self.session_ended(window, cx),
            Failure::SignIn => "Sign in again to continue.".into(),
            Failure::UsageLimit => {
                open_usage_limit(window, cx);
                // Kept inline too, so the turn can be retried once the limit resets.
                "Usage limit reached.".into()
            }
            Failure::Other(message) => message,
        };
        self.error = Some(TurnError {
            message: message.into(),
            retry,
        });
        cx.notify();
    }

    pub(super) fn retry(&mut self, retry: Retry, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        match retry {
            Retry::Models => self.load_models(window, cx),
            Retry::Reply => self.regenerate(window, cx),
        }
        cx.notify();
    }

    /// Drops the trailing reply (finished, partial or failed) and asks again.
    pub(super) fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Without a model the reply would be dropped and never replaced.
        if self.reply.is_some() || self.settings.model.is_none() {
            return;
        }
        while self
            .chat
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Assistant)
        {
            self.chat.messages.pop();
        }
        if !self.chat.messages.is_empty() {
            self.start_reply(window, cx);
        }
    }

    fn save_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat.messages.is_empty() {
            return;
        }
        self.chat.updated_at = unix_now();
        let (store, chat) = (self.store.clone(), self.chat.clone());
        let saved = self.disk.run(move || store.save(&chat));
        cx.spawn_in(window, async move |this, cx| {
            if let Some(Err(e)) = saved.await {
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(format!("Couldn't save this chat: {e}"), cx)
                });
            }
        })
        .detach();
        self.chats.retain(|c| c.id != self.chat.id);
        self.chats.insert(
            0,
            ChatSummary {
                id: self.chat.id.clone(),
                title: self.chat.title.clone(),
                updated_at: self.chat.updated_at,
            },
        );
    }

    /// Tells the virtual list how many rows the open chat has now.
    fn sync_rows(&self, cx: &mut Context<Self>) {
        let count = self.chat.messages.len();
        self.scroller.update(cx, |rows, cx| {
            let old = rows.item_count();
            if count > old {
                rows.append(count - old, cx);
            } else if count < old {
                rows.splice(count..old, 0, cx);
            }
            if count > 0 {
                rows.remeasure_items(count - 1..count, cx);
            }
        });
    }

    fn show_chat(&mut self, chat: Chat, window: &mut Window, cx: &mut Context<Self>) {
        self.chat = chat;
        self.error = None;
        self.ensure_models(window, cx);
        let count = self.chat.messages.len();
        self.scroller.update(cx, |rows, cx| rows.reset(count, cx));
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.signed_in() {
            return;
        }
        self.finish_reply(window, cx);
        let chat = self.store.new_chat();
        self.show_chat(chat, window, cx);
    }

    pub(super) fn open_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat.id == id {
            return;
        }
        self.finish_reply(window, cx);
        let (store, id) = (self.store.clone(), id.to_string());
        let loaded = self.disk.run(move || store.load(&id));
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(result) = loaded.await else { return };
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(chat) => this.show_chat(chat, window, cx),
                Err(e) => window.push_notification(format!("Couldn't open this chat: {e}"), cx),
            });
        }));
    }

    /// Opens the chat above (`-1`) or below (`1`) the open one in the sidebar.
    fn step_chat(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.signed_in() {
            return;
        }
        let target = match self.chats.iter().position(|c| c.id == self.chat.id) {
            Some(ix) => ix.checked_add_signed(step),
            // A new chat isn't listed until it has a message; it sits above the first row.
            None => (step > 0).then_some(0),
        };
        if let Some(chat) = target.and_then(|ix| self.chats.get(ix)) {
            let id = chat.id.clone();
            self.open_chat(&id, window, cx);
        }
    }

    /// Asks before deleting the open chat; a new chat has nothing on disk to delete.
    fn delete_open_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.signed_in() && self.chats.iter().any(|c| c.id == self.chat.id) {
            let (id, title) = (self.chat.id.clone(), self.chat.title.clone());
            self.confirm_delete(id, title, window, cx);
        }
    }

    pub(super) fn delete_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat.id == id {
            self.finish_reply(window, cx);
            let chat = self.store.new_chat();
            self.show_chat(chat, window, cx);
        }
        self.chats.retain(|c| c.id != id);
        let (store, id, drafts) = (self.store.clone(), id.to_string(), self.pending.clone());
        let deleted = self.disk.run(move || delete_keeping(&store, &id, &drafts));
        cx.spawn_in(window, async move |this, cx| {
            if let Some(Err(e)) = deleted.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    window.push_notification(format!("Couldn't delete this chat: {e}"), cx);
                    this.chats = this.store.list();
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// Quitting mid-reply keeps the text that already arrived, as Stop does, and waits for
    /// queued writes, since the process ends right after.
    fn save_before_exit(&mut self) {
        if let Some(reply) = self.reply.take() {
            reply.cancel.store(true, Ordering::Relaxed);
            if self
                .chat
                .messages
                .last()
                .is_some_and(|m| m.role == Role::Assistant && m.text.is_empty())
            {
                self.chat.messages.pop();
            }
            self.chat.updated_at = unix_now();
            let (store, chat) = (self.store.clone(), self.chat.clone());
            self.disk.queue(move || {
                let _ = store.save(&chat);
            });
        }
        if self.instructions_save.take().is_some() {
            self.save_settings();
        }
        self.disk.flush();
    }

    pub(super) fn toggle_web_search(&mut self, cx: &mut Context<Self>) {
        self.settings.web_search = !self.settings.web_search;
        self.save_settings();
        cx.notify();
    }
}

impl Render for Patina {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("patina")
            .key_context("Patina")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &NewChat, window, cx| this.new_chat(window, cx)))
            .on_action(
                cx.listener(|this, _: &DeleteChat, window, cx| this.delete_open_chat(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousChat, window, cx| this.step_chat(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &NextChat, window, cx| this.step_chat(1, window, cx)))
            .on_action(cx.listener(|this, _: &Stop, window, cx| this.finish_reply(window, cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .on_action(|_: &About, window, cx| open_about(window, cx))
            .map(|this| {
                if self.signed_in() {
                    this.child(
                        h_flex()
                            .size_full()
                            .items_stretch()
                            .child(self.render_sidebar(window, cx))
                            .child(self.render_chat(window, cx)),
                    )
                } else {
                    this.child(self.render_sign_in(window, cx))
                }
            })
    }
}
