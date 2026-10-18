//! File ▸ Send as email…: the opt-in step that delivers a document as a message, and the only
//! place the interface opens a socket.
//!
//! The message is the one File ▸ Save as email message… writes — the same engine call
//! (`Session::email_message`), so what a receiver keeps is byte for byte the `.eml` — and all this
//! adds is the SMTP session `doc_email_send` performs (AGENTS.md §3: everything the interface can do
//! is reachable headlessly, and both go through the same code).
//!
//! **Nothing connects until the user presses Send or Check connection.** Opening the dialog,
//! filling it in and cancelling it opens no socket at all; the session itself runs off the frame
//! thread, so a relay that stalls cannot freeze the window. Credentials live in the draft for this
//! session only: they are masked in the field, never written to the settings, never logged (the SMTP
//! client redacts the `AUTH` token from its session log) and never put in a notice.
//!
//! **The server is filled in, not demanded.** The provider the `From` address belongs to comes from
//! `pdfcraft_smtp::PRESETS` (one entry per provider), so typing `someone@gmail.com` puts
//! `smtp.gmail.com`, `465` and `TLS` in the boxes and Gmail's app-password requirement on screen
//! before the first attempt. Every box stays editable and an edit is never overwritten; a domain no
//! provider is listed for fills in nothing.
//!
//! **A password can be remembered, opt-in.** «Remember password» is off unless the user turns it on,
//! and nothing is written when it is turned on: the password is filed only after a send the relay
//! accepted, in the system's own secret store (`pdfcraft_platform::secrets`), under a key built from
//! the transport, the server, the port and the user name — so another server, port or user name
//! never picks it up. Opening the dialog asks the store for that key and puts what it finds in the
//! masked field, with «Forget password» to delete it. A store that is missing, locked or refusing is
//! said in place: the dialog stays usable and the password is simply typed by hand.

use std::sync::Arc;

use egui::{Align, Layout};
use pdfcraft_platform::secrets::{SecretStore, StoreError};

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, widgets};

/// How the connection is protected, as the dialog offers it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// No transport security: for a relay on this machine, or one already inside a trusted network.
    #[default]
    None,
    /// `STARTTLS` (RFC 3207): the session starts in the clear and is upgraded before the envelope.
    StartTls,
    /// Implicit TLS (RFC 8314, `smtps:`): the handshake comes before the greeting.
    Tls,
}

impl Transport {
    /// The port a relay of this kind listens on, used when the user leaves the port box empty.
    /// A default derived from the chosen transport — never from the machine's configuration.
    fn port(self) -> u16 {
        match self {
            Transport::None => 25,
            Transport::StartTls => 587,
            Transport::Tls => 465,
        }
    }

    /// The name the SMTP client knows this transport by.
    fn client(self) -> pdfcraft_smtp::Security {
        match self {
            Transport::None => pdfcraft_smtp::Security::Plain,
            Transport::StartTls => pdfcraft_smtp::Security::StartTls,
            Transport::Tls => pdfcraft_smtp::Security::ImplicitTls,
        }
    }

    /// The word that names this transport in the key a password is filed under.
    fn scheme(self) -> &'static str {
        match self {
            Transport::None => "plain",
            Transport::StartTls => "starttls",
            Transport::Tls => "tls",
        }
    }

    /// The transport a relay's security mode is offered as: the inverse of [`Transport::client`],
    /// so a provider's entry in the map fills the same box the user would set by hand.
    fn of(security: pdfcraft_smtp::Security) -> Transport {
        match security {
            pdfcraft_smtp::Security::Plain => Transport::None,
            pdfcraft_smtp::Security::StartTls => Transport::StartTls,
            pdfcraft_smtp::Security::ImplicitTls => Transport::Tls,
        }
    }
}

/// Which of the server boxes a person has typed into since a preset last filled them. A preset
/// fills only the others, so an edit is never overwritten by a later address.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Hand {
    host: bool,
    port: bool,
    transport: bool,
}

/// What the dialog holds. Every field starts empty: nothing is taken from the machine's settings,
/// the user's other mail clients or the document.
///
/// The server boxes may be filled in from the provider the `From` address belongs to; which of them
/// came from that and which a person typed is kept beside the draft (see [`Providers`]), so a draft
/// is only ever what the user sees.
#[derive(Clone, Default, PartialEq)]
pub struct Draft {
    pub host: String,
    /// Left empty, it means the usual port for the chosen transport ([`Transport::port`]).
    pub port: String,
    pub transport: Transport,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    /// The server asks for a password (`AUTH PLAIN`); the two fields below are only sent with this on.
    pub auth: bool,
    pub username: String,
    /// This session only: never saved, never logged, never shown (the field is masked).
    pub password: String,
}

/// The provider bookkeeping of one dialog: which boxes a person typed into, and which preset last
/// filled them. Kept beside the draft rather than in it, so the draft stays the fields themselves.
#[derive(Default)]
struct Providers {
    /// The `From` value the provider was last looked up for, so the domain is read once per edit
    /// rather than once per frame.
    synced_from: String,
    /// The boxes a person typed into: a preset leaves these alone.
    hand: Hand,
    /// The provider that last filled the boxes, so an address from a domain nobody lists can take
    /// back exactly what it put there.
    applied: Option<&'static pdfcraft_smtp::Preset>,
}

/// `Debug` never prints the password: a draft can reach a log through a panic message or a debug
/// print, and a credential must not travel that way (AGENTS.md §3).
impl std::fmt::Debug for Draft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draft")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("transport", &self.transport)
            .field("from", &self.from)
            .field("to", &self.to)
            .field("cc", &self.cc)
            .field("subject", &self.subject)
            .field("body", &self.body)
            .field("auth", &self.auth)
            .field("username", &self.username)
            .field("password", &if self.password.is_empty() { "" } else { "<hidden>" })
            .finish()
    }
}

impl Draft {
    /// The key the password for these boxes is filed under, or `None` while there is nothing a
    /// password could belong to.
    ///
    /// The transport, the server, the port and the user name are all in it (`pdfcraft:smtp:<transport>
    /// ://<host>:<port>/<username>`), so a different server, a different port, a different transport
    /// or a different login asks the store about a different secret: one relay's password is never
    /// sent to another. The port is the one the session would use, so an empty port box and a typed
    /// `587` name the same relay.
    pub fn secret_key(&self) -> Option<String> {
        if !self.auth {
            return None;
        }
        let host = self.host.trim();
        let username = self.username.trim();
        if host.is_empty() || username.is_empty() {
            return None;
        }
        let port = self.port().ok()?;
        Some(format!("pdfcraft:smtp:{}://{host}:{port}/{username}", self.transport.scheme()))
    }

    /// The port to connect to: the box's number, or the transport's usual one when it is empty.
    fn port(&self) -> Result<u16, String> {
        let text = self.port.trim();
        if text.is_empty() {
            return Ok(self.transport.port());
        }
        text.parse::<u16>().ok().filter(|port| *port != 0).ok_or_else(|| tl!("The port is a number from 1 to 65535").to_string())
    }

    /// The relay these boxes name, with everything checked that must hold before a socket is worth
    /// opening. Send and Check connection both start here, so the same mistake is the same reason.
    fn relay(&self) -> Result<pdfcraft_smtp::Server, String> {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            return Err(tl!("The server needs a host name").to_string());
        }
        let port = self.port()?;
        if self.transport == Transport::None && (!self.username.trim().is_empty() || !self.password.is_empty()) {
            return Err(tl!("A password needs an encrypted connection: choose STARTTLS or TLS").to_string());
        }
        let (username, password) =
            if self.auth && !self.username.trim().is_empty() { (Some(self.username.clone()), Some(self.password.clone())) } else { (None, None) };
        Ok(pdfcraft_smtp::Server {
            host,
            port,
            security: self.transport.client(),
            username,
            password,
            timeout: std::time::Duration::from_secs(30),
            client_name: "pdfcraft.local".to_string(),
            crlf: false,
            extra_roots: Vec::new(),
        })
    }
}

impl Providers {
    /// Fill the server boxes in from the provider the `From` address belongs to.
    ///
    /// Run once per frame before the boxes are drawn, and once per edit of `From`. A domain no
    /// provider is listed for leaves them empty — what a previous preset put there is taken back,
    /// while anything a person typed is kept.
    fn sync(&mut self, draft: &mut Draft) {
        if self.synced_from == draft.from {
            return;
        }
        self.synced_from = draft.from.clone();
        match pdfcraft_smtp::preset(&draft.from) {
            Some(entry) => self.fill(draft, entry, true),
            None => self.clear(draft),
        }
    }

    /// Put a provider's server, port and transport in the boxes. With `sparingly` the boxes a
    /// person typed into keep what they typed (the automatic path, when the address names the
    /// provider); choosing the provider from the list applies all three.
    fn fill(&mut self, draft: &mut Draft, entry: &'static pdfcraft_smtp::Preset, sparingly: bool) {
        if !(sparingly && self.hand.host) {
            draft.host = entry.host.to_string();
            self.hand.host = false;
        }
        if !(sparingly && self.hand.port) {
            draft.port = entry.port.to_string();
            self.hand.port = false;
        }
        if !(sparingly && self.hand.transport) {
            draft.transport = Transport::of(entry.security);
            self.hand.transport = false;
        }
        self.applied = Some(entry);
    }

    /// Take back what the last preset filled in, and nothing else.
    fn clear(&mut self, draft: &mut Draft) {
        let Some(entry) = self.applied.take() else { return };
        if !self.hand.host && draft.host == entry.host {
            draft.host.clear();
        }
        if !self.hand.port && draft.port == entry.port.to_string() {
            draft.port.clear();
        }
        if !self.hand.transport && draft.transport == Transport::of(entry.security) {
            draft.transport = Transport::default();
        }
    }

    /// The provider whose application-password page this dialog points at, and that page: the one
    /// part of the hint a person cannot guess. Empty for a provider that takes the account password.
    fn app_password(&self) -> Option<(&'static pdfcraft_smtp::Preset, &'static str)> {
        let entry = self.applied?;
        Some((entry, entry.app_password_page?))
    }
}

/// What one session did, kept for the notice and for the tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The relay took the message for these recipients (and refused the ones listed, if any).
    Delivered { host: String, port: u16, accepted: Vec<String>, refused: Vec<(String, String)>, bytes: usize, reply: String },
    /// Nothing was delivered, and this says why in words a person can act on.
    Failed(String),
}

/// What one connection check found. A check sends no message, so this says what it could prove:
/// the relay was reached, the session was encrypted as asked and the login was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The relay answered as asked and accepted the login (or none was given).
    Ok {
        host: String,
        port: u16,
        /// Whether a password was actually presented and accepted (`false` when none was entered).
        authenticated: bool,
    },
    /// The certificate did not check out: the relay is not the one its name claims.
    Certificate(String),
    /// The relay answered and refused the login: a wrong password, or one the provider no longer
    /// accepts (Gmail refuses an account password).
    Rejected(String),
    /// The relay could not be reached, or answered something that is not SMTP.
    Failed(String),
}

/// The one place an `Error` becomes the sentence the dialog shows.
fn checked(outcome: Result<pdfcraft_smtp::Checked, pdfcraft_smtp::Error>) -> CheckOutcome {
    match outcome {
        Ok(checked) => CheckOutcome::Ok { host: checked.host, port: checked.port, authenticated: checked.authenticated },
        Err(error) => match &error {
            pdfcraft_smtp::Error::Tls(reason) => CheckOutcome::Certificate(reason.clone()),
            pdfcraft_smtp::Error::Refused { command, .. } if command.starts_with("AUTH") => CheckOutcome::Rejected(error.to_string()),
            _ => CheckOutcome::Failed(error.to_string()),
        },
    }
}

/// What the dialog says about the password store, in a wording the renderer translates. Kept as a
/// kind rather than a finished sentence so the line reads in the interface's language.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Note {
    /// A password for these settings is filed, and it is in the field.
    Filed,
    /// This platform has no password store: remembering is not available at all.
    Unsupported,
    /// The store is there but locked.
    Locked,
    /// The store refused this exchange; the string is its own detail.
    Failed(String),
    /// The password could not be filed after a send the relay accepted.
    Unsaved(String),
}

impl Note {
    fn of(error: &StoreError) -> Self {
        match error {
            StoreError::Unsupported => Note::Unsupported,
            StoreError::Locked => Note::Locked,
            StoreError::Unavailable(reason) => Note::Failed(reason.clone()),
        }
    }
}

/// What the store answered, for the one notice the app shows. A refusal is already on screen in the
/// dialog; a save and a deletion are worth saying because the dialog may close in the same frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// The password was filed.
    Kept,
    /// The filed password was deleted.
    Forgotten,
    /// A send the relay accepted, but the password could not be filed; the reason is the store's.
    SaveFailed(String),
    /// The password could not be deleted; the reason is the store's.
    DeleteFailed(String),
}

/// What the dialog asks the store to do. It is answered on a worker thread: the bus is local and
/// fast, but it is not something a frame may wait on.
enum Request {
    /// Is a password filed under this key?
    Look { generation: u64, key: String },
    /// File this password under this key.
    Keep { generation: u64, key: String, secret: String },
    /// Delete what is filed under this key.
    Forget { generation: u64, key: String },
}

impl Request {
    fn generation(&self) -> u64 {
        match self {
            Request::Look { generation, .. } | Request::Keep { generation, .. } | Request::Forget { generation, .. } => *generation,
        }
    }

    fn key(&self) -> &str {
        match self {
            Request::Look { key, .. } | Request::Keep { key, .. } | Request::Forget { key, .. } => key,
        }
    }
}

/// Which request an answer belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Look,
    Keep,
    Forget,
}

/// What the store answered. It never carries the password: the field already has it, and an answer
/// can reach a log through a panic message.
#[derive(Debug)]
struct Answer {
    generation: u64,
    key: String,
    kind: Kind,
    result: Result<Option<String>, StoreError>,
}

/// The one thread that talks to the store, and the queue of work between it and the frame. Started
/// by the first request and never restarted: a dialog that opens, asks nothing and closes leaves no
/// thread behind.
#[derive(Default)]
struct Vault {
    requests: Option<std::sync::mpsc::Sender<Request>>,
    answers: Option<std::sync::mpsc::Receiver<Answer>>,
}

impl Vault {
    /// Hand a request to the worker, starting it if this is the first. `Err` only when no thread can
    /// be started at all, which the dialog says rather than silently doing nothing.
    fn ask(&mut self, store: &Arc<dyn SecretStore>, ctx: &Option<egui::Context>, request: Request) -> Result<(), ()> {
        if self.requests.is_none() {
            let (requests, incoming) = std::sync::mpsc::channel::<Request>();
            let (outgoing, answers) = std::sync::mpsc::channel::<Answer>();
            let store = Arc::clone(store);
            let ctx = ctx.clone();
            let worker = std::thread::Builder::new().name("pdfcraft-email-store".to_string()).spawn(move || {
                while let Ok(request) = incoming.recv() {
                    let answer = answer(&store, &request);
                    let _ = outgoing.send(answer);
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                }
            });
            if worker.is_err() {
                return Err(());
            }
            self.requests = Some(requests);
            self.answers = Some(answers);
        }
        self.requests.as_ref().ok_or(())?.send(request).map_err(|_| ())
    }

    fn next(&mut self) -> Option<Answer> {
        self.answers.as_mut()?.try_recv().ok()
    }
}

/// One exchange with the store, on the worker thread.
fn answer(store: &Arc<dyn SecretStore>, request: &Request) -> Answer {
    let (kind, result) = match request {
        Request::Look { key, .. } => (Kind::Look, store.get(key)),
        Request::Keep { key, secret, .. } => (Kind::Keep, store.set(key, secret).map(|()| None)),
        Request::Forget { key, .. } => (Kind::Forget, store.delete(key).map(|()| None)),
    };
    Answer { generation: request.generation(), key: request.key().to_string(), kind, result }
}

/// A send that is still running, and what to file if the relay takes the message.
struct Sending {
    outcome: std::sync::mpsc::Receiver<Outcome>,
    /// The key and the password to file once the relay has accepted the message. `None` unless
    /// «Remember password» was on when Send was pressed.
    keep: Option<(String, String)>,
}

/// The dialog's state and the sessions it started.
pub struct EmailSend {
    pub draft: Draft,
    /// The last session's outcome (`None` before the first one).
    pub outcome: Option<Outcome>,
    /// The last connection check (`None` before the first one).
    pub checked: Option<CheckOutcome>,
    /// «Remember password»: off until the user turns it on, and only ever acted on after a send the
    /// relay accepted. A test replaces the store with an in-memory one.
    pub store: Arc<dyn SecretStore>,
    pub remember: bool,
    /// Which boxes a person typed into, and which provider last filled the others.
    providers: Providers,
    /// The key the draft's settings name right now, and whether the store has a password filed
    /// under it that this dialog knows about.
    key: Option<String>,
    filed: bool,
    /// What the dialog says about the store, under the password field.
    note: Option<Note>,
    /// Counts requests, so an answer to a question a later edit made pointless can be told apart
    /// from the one the dialog is waiting for.
    generation: u64,
    /// The lookup that is in flight, if any, and the key a frame asked about while it ran.
    awaiting: Option<u64>,
    deferred: Option<String>,
    vault: Vault,
    /// The session running off the frame thread.
    running: Option<Sending>,
    /// The connection check running off the frame thread.
    checking: Option<std::sync::mpsc::Receiver<CheckOutcome>>,
}

impl Default for EmailSend {
    fn default() -> Self {
        Self {
            draft: Draft::default(),
            outcome: None,
            checked: None,
            store: pdfcraft_platform::secrets::system(),
            remember: false,
            providers: Providers::default(),
            key: None,
            filed: false,
            note: None,
            generation: 0,
            awaiting: None,
            deferred: None,
            vault: Vault::default(),
            running: None,
            checking: None,
        }
    }
}

impl EmailSend {
    /// Follow the key the boxes name, once per frame. When it changes, the password filed under the
    /// old key is not this session's — a value that came from the store is cleared, a value a person
    /// typed is kept — and the store is asked about the new one. One lookup at a time; the newest key
    /// wins, so holding down a key in the server box does not queue a request per keystroke.
    fn track(&mut self, ctx: &Option<egui::Context>) {
        let key = self.draft.secret_key();
        if key == self.key {
            return;
        }
        if self.filed {
            self.draft.password.clear();
        }
        self.filed = false;
        self.note = None;
        self.key = key;
        if let Some(key) = self.key.clone() {
            self.look(&key, ctx);
        }
    }

    fn look(&mut self, key: &str, ctx: &Option<egui::Context>) {
        if self.awaiting.is_some() {
            self.deferred = Some(key.to_string());
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let request = Request::Look { generation, key: key.to_string() };
        match self.vault.ask(&self.store, ctx, request) {
            Ok(()) => self.awaiting = Some(generation),
            Err(()) => self.note = Some(Note::Failed("the store worker could not be started".to_string())),
        }
    }

    /// File a password the relay has just accepted. Only a send the relay took reaches here, so a
    /// mistyped password leaves nothing behind.
    fn keep(&mut self, key: &str, secret: &str, ctx: &Option<egui::Context>) {
        self.generation += 1;
        let generation = self.generation;
        let request = Request::Keep { generation, key: key.to_string(), secret: secret.to_string() };
        if self.vault.ask(&self.store, ctx, request).is_err() {
            self.note = Some(Note::Unsaved("the store worker could not be started".to_string()));
        }
    }

    /// Delete what is filed under the current key. The field is cleared at once — the user asked for
    /// the password to be gone — and the store's answer says whether it went.
    fn forget(&mut self, ctx: &Option<egui::Context>) {
        let Some(key) = self.key.clone() else { return };
        self.draft.password.clear();
        self.filed = false;
        self.generation += 1;
        let generation = self.generation;
        let request = Request::Forget { generation, key };
        if self.vault.ask(&self.store, ctx, request).is_err() {
            self.note = Some(Note::Failed("the store worker could not be started".to_string()));
        }
    }

    /// Whether a lookup for the current settings is still on its way. The dialog does not act on
    /// this; it exists because the state is otherwise unobservable, and a test waits on it rather
    /// than on a timer.
    pub fn store_pending(&self) -> bool {
        self.awaiting.is_some() || self.deferred.is_some()
    }

    /// Take in what the store has answered (every frame) and say what the app should announce.
    fn poll(&mut self, ctx: &Option<egui::Context>) -> Vec<Said> {
        let mut said = Vec::new();
        while let Some(answer) = self.vault.next() {
            if self.awaiting == Some(answer.generation) {
                self.awaiting = None;
            }
            let current = self.key.as_deref() == Some(answer.key.as_str());
            let result = answer.result;
            match (answer.kind, result) {
                (Kind::Look, Ok(Some(secret))) => {
                    // A password a person typed is theirs: the store never overwrites it.
                    if current && self.draft.password.is_empty() {
                        self.draft.password = secret;
                        self.filed = true;
                        self.note = Some(Note::Filed);
                    }
                }
                (Kind::Look, Ok(None)) => {
                    if current {
                        self.filed = false;
                        self.note = None;
                    }
                }
                (Kind::Look, Err(error)) => {
                    if current {
                        self.filed = false;
                        self.note = Some(Note::of(&error));
                    }
                }
                (Kind::Keep, Ok(_)) => {
                    if current {
                        self.filed = true;
                        self.note = Some(Note::Filed);
                    }
                    said.push(Said::Kept);
                }
                (Kind::Keep, Err(error)) => {
                    self.note = Some(Note::Unsaved(error.to_string()));
                    said.push(Said::SaveFailed(error.to_string()));
                }
                (Kind::Forget, Ok(_)) => {
                    if current {
                        self.filed = false;
                        self.note = None;
                    }
                    said.push(Said::Forgotten);
                }
                (Kind::Forget, Err(error)) => {
                    self.note = Some(Note::of(&error));
                    said.push(Said::DeleteFailed(error.to_string()));
                }
            }
        }
        if self.awaiting.is_none()
            && let Some(key) = self.deferred.take()
            && self.key.as_deref() == Some(key.as_str())
        {
            self.look(&key, ctx);
        }
        said
    }
}

/// What the dialog's buttons ask the app to do. The caller runs it, so a reason to stay open can be
/// shown without leaving the dialog.
#[derive(Default)]
pub(crate) struct Actions {
    pub send: bool,
    pub check: bool,
    pub cancel: bool,
    /// «Forget password»: delete what the store holds for these settings.
    pub forget: bool,
    /// A page to open in the system browser: the provider's application-password page.
    pub open: Option<&'static str>,
}

impl PdfCraftApp {
    /// File ▸ Send as email…: open the dialog. It opens no socket by itself; the relay is only
    /// contacted by the Send and Check connection buttons (see [`Self::send_email`] and
    /// [`Self::check_email`]).
    pub(crate) fn email_send_dialog(&mut self) {
        if self.active_ids().is_none() {
            return;
        }
        self.dialog = Some(crate::Dialog::EmailSend);
    }

    /// The dialog's Send button: check the envelope, compose the message and hand it to the relay on
    /// a worker thread.
    ///
    /// `Err` is a reason (already in the interface language) to keep the dialog open: nothing was
    /// composed and **no socket was opened**. Every address is checked here, before the message is
    /// rendered, so a typo never costs a page-rendering run.
    pub fn send_email(&mut self) -> Result<(), String> {
        if self.email.running.is_some() {
            return Err(tl!("The previous message is still being sent").to_string());
        }
        let Some((_, id)) = self.active_ids() else {
            return Err(tl!("Open a document first").to_string());
        };
        let draft = self.email.draft.clone();
        let server = draft.relay()?;
        let from = pdfcraft_smtp::address("from", &draft.from).map_err(|e| e.to_string())?;
        let mut recipients: Vec<String> = Vec::new();
        for list in [&draft.to, &draft.cc] {
            for part in list.split([',', ';', '\n']) {
                if part.trim().is_empty() {
                    continue;
                }
                recipients.push(pdfcraft_smtp::address("recipient", part).map_err(|e| e.to_string())?);
            }
        }
        if recipients.is_empty() {
            return Err(tl!("A message needs at least one recipient").to_string());
        }

        // The same call File ▸ Save as email message… makes: the bytes are the .eml, and a document
        // that cannot be rendered fails here, before anything is connected.
        let options = pdfcraft_engine::EmailOptions {
            from: from.clone(),
            to: draft.to.trim().to_string(),
            cc: draft.cc.trim().to_string(),
            subject: draft.subject.clone(),
            body: draft.body.clone(),
            dpi: 96.0,
        };
        let bytes = self.session.email_message(id, &options).map_err(|e| e.to_string())?;

        // What to file once the relay takes the message — and only then: a password the server
        // refused must leave nothing in the store.
        let keep = if self.email.remember && !draft.password.is_empty() { draft.secret_key().map(|key| (key, draft.password.clone())) } else { None };

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let outcome = match pdfcraft_smtp::send(&server, &from, &recipients, &bytes) {
                Ok(report) => Outcome::Delivered {
                    host: report.host,
                    port: report.port,
                    accepted: report.accepted,
                    refused: report.refused,
                    bytes: report.bytes,
                    reply: report.reply,
                },
                Err(error) => Outcome::Failed(error.to_string()),
            };
            // The receiver may be gone (the app quit): nothing to report to then.
            let _ = tx.send(outcome);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.email.running = Some(Sending { outcome: rx, keep });
        self.email.outcome = None;
        Ok(())
    }

    /// The dialog's Check connection button: the session `send` would open — greeting, `EHLO`,
    /// the transport as chosen, `AUTH PLAIN` when a password was given — and `QUIT` after it. No
    /// message is composed and nothing is handed over, so a user finds out about a wrong port, a
    /// certificate that does not check out or a password the provider refuses **before** writing a
    /// message, and a relay has nothing to deliver either way.
    pub fn check_email(&mut self) -> Result<(), String> {
        if self.email.checking.is_some() || self.email.running.is_some() {
            return Err(tl!("A session is already running; wait for it to finish").to_string());
        }
        let server = self.email.draft.relay()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let outcome = checked(pdfcraft_smtp::verify(&server));
            let _ = tx.send(outcome);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.email.checking = Some(rx);
        self.email.checked = None;
        Ok(())
    }

    /// Pick up a finished session and say what happened (each frame). A relay that refused some
    /// recipients, a certificate that did not check out and a server that never answered all end up
    /// in a notice with the reason, never in silence.
    pub(crate) fn poll_email_send(&mut self) {
        {
            let Some(mut sending) = self.email.running.take() else { return };
            let outcome = match sending.outcome.try_recv() {
                Ok(outcome) => outcome,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.email.running = Some(sending);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Outcome::Failed(tl!("the session stopped unexpectedly").to_string()),
            };
            // The relay took the message, so this is the first moment the password may be filed.
            if matches!(outcome, Outcome::Delivered { .. })
                && let Some((key, secret)) = sending.keep.take()
            {
                let ctx = self.ctx.clone();
                self.email.keep(&key, &secret, &ctx);
            }
            match &outcome {
                Outcome::Delivered { host, port, refused, bytes, .. } => {
                    let server = format!("{host}:{port}");
                    let size = bytes.to_string();
                    if refused.is_empty() {
                        self.notify_fmt("Message sent to {server} ({bytes} bytes)", &[("server", &server), ("bytes", &size)]);
                    } else {
                        let names = refused.iter().map(|(address, _)| address.as_str()).collect::<Vec<_>>().join(", ");
                        self.notify_fmt(
                            "Message sent to {server} ({bytes} bytes); the relay refused {refused}",
                            &[("server", &server), ("bytes", &size), ("refused", &names)],
                        );
                    }
                }
                Outcome::Failed(reason) => self.notify_fmt("Not sent: {reason}", &[("reason", reason)]),
            }
            self.email.outcome = Some(outcome);
        }
    }

    /// Pick up a finished connection check and say what it proved, or how it failed.
    pub(crate) fn poll_email_check(&mut self) {
        let Some(rx) = self.email.checking.take() else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.email.checking = Some(rx);
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => CheckOutcome::Failed(tl!("the check stopped unexpectedly").to_string()),
        };
        match &outcome {
            CheckOutcome::Ok { host, port, authenticated } => {
                let server = format!("{host}:{port}");
                if *authenticated {
                    self.notify_fmt("Connection to {server} is fine, and the login was accepted", &[("server", &server)]);
                } else {
                    self.notify_fmt("Connection to {server} is fine (no password was checked)", &[("server", &server)]);
                }
            }
            CheckOutcome::Certificate(reason) => self.notify_fmt("The certificate could not be checked: {reason}", &[("reason", reason)]),
            CheckOutcome::Rejected(reason) => self.notify_fmt("The server refused the login: {reason}", &[("reason", reason)]),
            CheckOutcome::Failed(reason) => self.notify_fmt("Could not connect: {reason}", &[("reason", reason)]),
        }
        self.email.checked = Some(outcome);
    }

    /// The dialog's «Forget password»: delete what the system store holds for these settings. The
    /// field is cleared at once — the user asked for the password to be gone — and the answer says
    /// whether the record went (see [`Self::poll_email_store`]).
    pub fn forget_email_password(&mut self) {
        let ctx = self.ctx.clone();
        self.email.forget(&ctx);
    }

    /// Pick up what the password store answered (every frame): a password to fill in, a refusal the
    /// dialog shows in place, or a save and a deletion worth announcing. The store is never asked
    /// anything from here — only the answers are read.
    pub(crate) fn poll_email_store(&mut self) {
        let ctx = self.ctx.clone();
        for said in self.email.poll(&ctx) {
            match said {
                Said::Kept => self.notify(tl!("The password is saved").to_string()),
                Said::Forgotten => self.notify(tl!("The password was forgotten").to_string()),
                Said::SaveFailed(reason) => self.notify_fmt("The password could not be saved: {reason}", &[("reason", &reason)]),
                Said::DeleteFailed(reason) => self.notify_fmt("The password could not be deleted: {reason}", &[("reason", &reason)]),
            }
        }
    }
}

/// The dialog's fields and buttons.
pub(crate) fn body(ui: &mut egui::Ui, email: &mut EmailSend, t: &Tokens) -> Actions {
    let mut actions = Actions::default();
    // The store's view follows the boxes: a changed server, port, transport or user name asks about
    // a different password, and answers that a later edit overtook never land in the fields.
    email.track(&Some(ui.ctx().clone()));
    let checking = email.checking.is_some();
    let EmailSend { draft, providers, remember, note, filed, .. } = email;
    providers.sync(draft);

    ui.label(egui::RichText::new(tl!("Send as email")).font(theme::semibold(18.0)));
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(tl!("The message is the one Save as email message… writes, delivered over SMTP. Nothing is sent until you press Send."))
            .size(12.0)
            .color(t.text_muted),
    );
    ui.add_space(8.0);
    egui::Grid::new("email-send").num_columns(2).spacing([12.0, 8.0]).min_col_width(88.0).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, label: &str, value: &mut String, width: f32| -> egui::Response {
            let l = ui.label(tl!(label));
            let response = ui.add(egui::TextEdit::singleline(value).desired_width(width)).labelled_by(l.id);
            ui.end_row();
            response
        };
        row(ui, "From:", &mut draft.from, 300.0);

        // The providers the map knows, so the server, the port and the transport can be picked
        // instead of remembered. Choosing one fills all three; every box stays editable after it.
        ui.label(tl!("Provider:"));
        let chosen = providers.applied.map_or("—", |entry| entry.name);
        egui::ComboBox::from_id_salt("email-provider").selected_text(chosen).show_ui(ui, |ui| {
            for entry in pdfcraft_smtp::PRESETS {
                let selected = providers.applied.is_some_and(|applied| applied.name == entry.name);
                if ui.selectable_label(selected, entry.name).clicked() {
                    providers.fill(draft, entry, false);
                }
            }
        });
        ui.end_row();

        if row(ui, "Server:", &mut draft.host, 300.0).changed() {
            providers.hand.host = true;
        }
        if row(ui, "Port:", &mut draft.port, 80.0).changed() {
            providers.hand.port = true;
        }
        ui.label(tl!("Security:"));
        let before = draft.transport;
        egui::ComboBox::from_id_salt("email-transport")
            .selected_text(match draft.transport {
                Transport::None => tl!("None"),
                Transport::StartTls => "STARTTLS",
                Transport::Tls => "TLS",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.transport, Transport::None, tl!("None"));
                ui.selectable_value(&mut draft.transport, Transport::StartTls, "STARTTLS");
                ui.selectable_value(&mut draft.transport, Transport::Tls, "TLS");
            });
        if draft.transport != before {
            providers.hand.transport = true;
        }
        ui.end_row();
        row(ui, "To:", &mut draft.to, 300.0);
        row(ui, "Cc:", &mut draft.cc, 300.0);
        row(ui, "Subject:", &mut draft.subject, 300.0);
    });
    if draft.transport == Transport::None {
        ui.label(egui::RichText::new(tl!("Not encrypted: the message travels in the clear")).size(12.0).color(t.text_muted));
    }
    // The one thing about a provider a person cannot work out from the dialog: it does not take the
    // account's own password, and where the one it does take is made.
    if let Some((entry, page)) = providers.app_password() {
        let typed = draft.from.trim();
        let account = if typed.is_empty() { entry.name } else { typed };
        ui.label(
            egui::RichText::new(crate::i18n::fmt(
                tl!("{provider} needs an app password, not the account's password (two-step verification must be on): account {address} → Security → App passwords."),
                &[("provider", entry.name), ("address", account)],
            ))
            .size(12.0)
            .color(t.text_muted),
        );
        if ui.link(tl!("Open app passwords")).clicked() {
            actions.open = Some(page);
        }
    }
    ui.add_space(4.0);
    ui.label(tl!("Body:"));
    ui.add(egui::TextEdit::multiline(&mut draft.body).desired_width(f32::INFINITY).desired_rows(4));
    ui.add_space(4.0);
    ui.add(widgets::checkbox(&mut draft.auth, tl!("The server needs a user name and password")));
    ui.add_enabled_ui(draft.auth, |ui| {
        egui::Grid::new("email-auth").num_columns(2).spacing([12.0, 8.0]).min_col_width(88.0).show(ui, |ui| {
            let l = ui.label(tl!("User name:"));
            ui.add(egui::TextEdit::singleline(&mut draft.username).desired_width(220.0)).labelled_by(l.id);
            ui.end_row();
            let l = ui.label(tl!("Password:"));
            ui.add(egui::TextEdit::singleline(&mut draft.password).password(true).desired_width(220.0)).labelled_by(l.id);
            ui.end_row();
            // The opt-in step: nothing is filed until this is on, and even then only after a send the
            // relay accepted. A platform with no password store cannot offer it at all.
            ui.label("");
            let usable = !matches!(note, Some(Note::Unsupported));
            ui.add_enabled(usable, widgets::checkbox(remember, tl!("Remember password")));
            ui.end_row();
        });
        // A password the store holds for these settings can be deleted from here.
        if *filed && widgets::pill_button(ui, tl!("Forget password"), false).clicked() {
            actions.forget = true;
        }
    });
    if draft.auth {
        // Where the password went, or why it did not: the field is always a password field, so the
        // one thing the dialog must not do is stay silent about a store it could not use.
        let muted = |ui: &mut egui::Ui, text: String| {
            ui.label(egui::RichText::new(text).size(12.0).color(t.text_muted));
        };
        match note {
            Some(Note::Filed) => muted(ui, tl!("The saved password is filled in").to_string()),
            Some(Note::Unsupported) => {
                muted(ui, tl!("This system has no password store: the password is only kept while this dialog is open").to_string())
            }
            Some(Note::Locked) => muted(ui, tl!("The keyring is locked, so the saved password could not be read").to_string()),
            Some(Note::Failed(reason)) => muted(ui, crate::i18n::fmt(tl!("The password store could not be used: {reason}"), &[("reason", reason)])),
            Some(Note::Unsaved(reason)) => muted(ui, crate::i18n::fmt(tl!("The password could not be saved: {reason}"), &[("reason", reason)])),
            None => {}
        }
    }
    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if widgets::pill_button(ui, tl!("Send"), true).clicked() {
            actions.send = true;
        }
        // The opt-in way to find out whether the settings work without writing a message.
        let label = if checking { tl!("Checking…") } else { tl!("Check connection") };
        ui.add_enabled_ui(!checking, |ui| {
            if widgets::pill_button(ui, label, false).clicked() {
                actions.check = true;
            }
        });
        if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
            actions.cancel = true;
        }
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_platform::secrets::MemoryStore;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn draft() -> Draft {
        Draft {
            host: "a.example".to_string(),
            port: String::new(),
            transport: Transport::StartTls,
            from: "ada@example.com".to_string(),
            to: "team@example.com".to_string(),
            auth: true,
            username: "user1".to_string(),
            password: "hunter2".to_string(),
            ..Draft::default()
        }
    }

    /// The key is the specification the store is filed under: transport, server, port and user name,
    /// so any change of them names a different password.
    #[test]
    fn the_key_names_the_transport_the_server_the_port_and_the_user() {
        assert_eq!(draft().secret_key().as_deref(), Some("pdfcraft:smtp:starttls://a.example:587/user1"));
        assert_eq!(Draft { transport: Transport::Tls, ..draft() }.secret_key().as_deref(), Some("pdfcraft:smtp:tls://a.example:465/user1"));
        assert_eq!(Draft { port: "2525".to_string(), ..draft() }.secret_key().as_deref(), Some("pdfcraft:smtp:starttls://a.example:2525/user1"));
        // Nothing a password could belong to: no login, or no server, or no password field at all.
        assert_eq!(Draft { username: "  ".to_string(), ..draft() }.secret_key(), None);
        assert_eq!(Draft { host: String::new(), ..draft() }.secret_key(), None);
        assert_eq!(Draft { auth: false, ..draft() }.secret_key(), None);
        // A port that is not a port names no relay, so it files nothing.
        assert_eq!(Draft { port: "not a port".to_string(), ..draft() }.secret_key(), None);
    }

    /// The whole point of the opt-in: the password is filed after a send the relay accepted, and a
    /// send it refused leaves nothing behind.
    #[test]
    fn a_send_the_relay_took_files_the_password_and_a_refused_one_leaves_nothing() {
        let key = "pdfcraft:smtp:starttls://a.example:587/user1".to_string();
        let delivered = Outcome::Delivered {
            host: "a.example".to_string(),
            port: 587,
            accepted: vec!["team@example.com".to_string()],
            refused: Vec::new(),
            bytes: 128,
            reply: "250 2.0.0 message accepted".to_string(),
        };

        for (outcome, filed) in [(delivered, true), (Outcome::Failed("the relay said no".to_string()), false)] {
            let store = Arc::new(MemoryStore::new());
            let mut app = PdfCraftApp::new();
            app.email.store = store.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            app.email.running = Some(Sending { outcome: rx, keep: Some((key.clone(), "hunter2".to_string())) });
            tx.send(outcome).unwrap();
            app.poll_email_send();
            // The store is spoken to off the frame thread, so give the worker the time it takes and
            // read its answer the way a frame does.
            let deadline = Instant::now() + if filed { Duration::from_secs(5) } else { Duration::from_millis(300) };
            while Instant::now() < deadline {
                app.poll_email_store();
                if filed && app.toast.as_ref().is_some_and(|toast| toast.0.contains("The password is saved")) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if filed {
                assert_eq!(store.peek(&key), Some("hunter2".to_string()), "the accepted send filed the password");
                let toast = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
                assert!(toast.contains("The password is saved"), "{toast}");
            } else {
                assert!(store.is_empty(), "a refused send must not leave a password in the store: {:?}", store.peek(&key));
                let toast = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
                assert!(!toast.contains("The password is saved"), "a refused send claimed to save the password: {toast}");
            }
        }
    }

    /// The wire between the frame and the worker carries no password in its diagnostic form: an
    /// answer can reach a log through a panic message.
    #[test]
    fn an_answer_never_carries_the_secret() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let request = Request::Keep { generation: 7, key: "pdfcraft:smtp:tls://a.example:465/user1".to_string(), secret: "hunter2".to_string() };
        let answer = answer(&store, &request);
        let shown = format!("{answer:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("pdfcraft:smtp:tls://a.example:465/user1"), "{shown}");
        // The request does carry it, so it must not print itself: nothing derives Debug for it.
        let _ = request;
    }

    /// A password the store refuses is the same refusal the dialog shows; the field is untouched.
    #[test]
    fn a_refusal_becomes_the_dialogs_own_note() {
        assert_eq!(Note::of(&StoreError::Unsupported), Note::Unsupported);
        assert_eq!(Note::of(&StoreError::Locked), Note::Locked);
        assert_eq!(Note::of(&StoreError::Unavailable("no session bus".to_string())), Note::Failed("no session bus".to_string()));
    }
}
