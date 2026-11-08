use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// The custom instructions field in Settings; edits are saved as they happen.
    pub(super) instructions: Entity<TextareaState>,
    focus_handle: FocusHandle,
    models_task: Option<Task<()>>,
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

impl From<AuthError> for Failure {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::SignInRequired => Failure::SignIn,
            e => Failure::Other(e.to_string()),
        }
    }
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        if e.is_auth() {
            Failure::SignIn
        } else if e.is_usage_limit() {
            Failure::UsageLimit
        } else {
            Failure::Other(e.message)
        }
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
        let turns = self
            .history
            .iter()
            .map(|m| self.turn(m))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| Failure::Other(format!("Couldn't read an attachment: {e}")))?;
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

    fn turn(&self, message: &Message) -> io::Result<api::Turn> {
        let attachments = message
            .attachments
            .iter()
            .map(|a| {
                Ok(api::Attachment {
                    name: a.name.clone(),
                    mime: a.mime.clone(),
                    data: self.store.attachment_bytes(a)?,
                })
            })
            .collect::<io::Result<_>>()?;
        Ok(api::Turn {
            role: match message.role {
                Role::User => api::Role::User,
                Role::Assistant => api::Role::Assistant,
            },
            text: message.text.clone(),
            attachments,
        })
    }
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
                        this.save_settings();
                    }
                }),
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
            focus_handle: cx.focus_handle(),
            models_task: None,
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
    /// `PATINA_PREVIEW_DIALOG` (settings, welcome, usage, about, delete) opens a dialog and
    /// `PATINA_PREVIEW_SIZE=720x480` resizes the window, so states that need input can be
    /// checked without a keyboard.
    fn run_preview_steps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preview_size = std::env::var("PATINA_PREVIEW_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some(size(px(w.parse().ok()?), px(h.parse().ok()?)))
        });
        if let Some(preview_size) = preview_size {
            window.resize(preview_size);
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
            self.focus_handle.focus(window, cx);
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
            Err(e) => self.sign_in.error = Some(e.to_string().into()),
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
        self.focus_handle.focus(window, cx);
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
                    this.sign_in.notice.get_or_insert_with(|| {
                        "Signed out. If this Mac was offline, also disconnect Patina in ChatGPT settings."
                            .into()
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn session_ended(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sign_out(window, cx);
        self.sign_in.notice = Some("Your session ended. Sign in again to continue.".into());
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
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(models) => this.set_models(models, window, cx),
                Err(failure) => this.fail(failure, Retry::Models, window, cx),
            });
        }));
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
        // ponytail: a failed settings write only loses a preference, so it is not surfaced.
        let _ = self.store.save_settings(&self.settings);
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
        let Some(model) = self.settings.model.clone() else {
            return;
        };
        self.error = None;
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
        self.save_chat(window, cx);
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
            Failure::SignIn => AuthError::SignInRequired.to_string(),
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
        if self.reply.is_some() {
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
        if let Err(e) = self.store.save(&self.chat) {
            window.push_notification(format!("Couldn't save this chat: {e}"), cx);
        }
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
        match self.store.load(id) {
            Ok(chat) => self.show_chat(chat, window, cx),
            Err(e) => window.push_notification(format!("Couldn't open this chat: {e}"), cx),
        }
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
        let open = self.chat.id == id;
        if open {
            self.finish_reply(window, cx);
        }
        if let Err(e) = self.store.delete(id) {
            return window.push_notification(format!("Couldn't delete this chat: {e}"), cx);
        }
        self.chats.retain(|c| c.id != id);
        if open {
            let chat = self.store.new_chat();
            self.show_chat(chat, window, cx);
        }
        cx.notify();
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
                    this.child(self.render_sign_in(cx))
                }
            })
    }
}
