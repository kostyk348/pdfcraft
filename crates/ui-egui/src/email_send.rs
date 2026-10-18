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

use egui::{Align, Layout};

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

    /// Fill the server boxes in from the provider the `From` address belongs to.
    ///
    /// Run once per frame before the boxes are drawn, and once per edit of `From`. A domain no
    /// provider is listed for leaves them empty — what a previous preset put there is taken back,
    /// while anything a person typed is kept.
    fn sync_preset(&mut self) {
        if self.synced_from == self.from {
            return;
        }
        self.synced_from = self.from.clone();
        match pdfcraft_smtp::preset(&self.from) {
            Some(preset) => self.fill(preset, true),
            None => self.clear_preset(),
        }
    }

    /// Put a provider's server, port and transport in the boxes. With `sparingly` the boxes a
    /// person typed into keep what they typed (the automatic path, when the address names the
    /// provider); choosing the provider from the list applies all three.
    fn fill(&mut self, preset: &'static pdfcraft_smtp::Preset, sparingly: bool) {
        if !(sparingly && self.hand.host) {
            self.host = preset.host.to_string();
            self.hand.host = false;
        }
        if !(sparingly && self.hand.port) {
            self.port = preset.port.to_string();
            self.hand.port = false;
        }
        if !(sparingly && self.hand.transport) {
            self.transport = Transport::of(preset.security);
            self.hand.transport = false;
        }
        self.applied = Some(preset);
    }

    /// Take back what the last preset filled in, and nothing else.
    fn clear_preset(&mut self) {
        let Some(preset) = self.applied.take() else { return };
        if !self.hand.host && self.host == preset.host {
            self.host.clear();
        }
        if !self.hand.port && self.port == preset.port.to_string() {
            self.port.clear();
        }
        if !self.hand.transport && self.transport == Transport::of(preset.security) {
            self.transport = Transport::default();
        }
    }

    /// The provider whose application-password page this draft points at, and that page: the one
    /// part of the hint a person cannot guess. Empty for a provider that takes the account password.
    fn app_password(&self) -> Option<(&'static pdfcraft_smtp::Preset, &'static str)> {
        let preset = self.applied?;
        Some((preset, preset.app_password_page?))
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

/// The dialog's state and the sessions it started.
#[derive(Default)]
pub struct EmailSend {
    pub draft: Draft,
    /// The last session's outcome (`None` before the first one).
    pub outcome: Option<Outcome>,
    /// The last connection check (`None` before the first one).
    pub checked: Option<CheckOutcome>,
    /// The session running off the frame thread.
    running: Option<std::sync::mpsc::Receiver<Outcome>>,
    /// The connection check running off the frame thread.
    checking: Option<std::sync::mpsc::Receiver<CheckOutcome>>,
}

/// What the dialog's buttons ask the app to do. The caller runs it, so a reason to stay open can be
/// shown without leaving the dialog.
#[derive(Default)]
pub(crate) struct Actions {
    pub send: bool,
    pub check: bool,
    pub cancel: bool,
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
        self.email.running = Some(rx);
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
            let Some(rx) = self.email.running.take() else { return };
            let outcome = match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.email.running = Some(rx);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Outcome::Failed(tl!("the session stopped unexpectedly").to_string()),
            };
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
}

/// The dialog's fields and buttons.
pub(crate) fn body(ui: &mut egui::Ui, email: &mut EmailSend, t: &Tokens) -> Actions {
    let mut actions = Actions::default();
    let checking = email.checking.is_some();
    let draft = &mut email.draft;
    draft.sync_preset();

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
        let chosen = draft.applied.map_or("—", |preset| preset.name);
        egui::ComboBox::from_id_salt("email-provider").selected_text(chosen).show_ui(ui, |ui| {
            for preset in pdfcraft_smtp::PRESETS {
                let selected = draft.applied.is_some_and(|applied| applied.name == preset.name);
                if ui.selectable_label(selected, preset.name).clicked() {
                    draft.fill(preset, false);
                }
            }
        });
        ui.end_row();

        if row(ui, "Server:", &mut draft.host, 300.0).changed() {
            draft.hand.host = true;
        }
        if row(ui, "Port:", &mut draft.port, 80.0).changed() {
            draft.hand.port = true;
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
            draft.hand.transport = true;
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
    if let Some((preset, page)) = draft.app_password() {
        let typed = draft.from.trim();
        let account = if typed.is_empty() { preset.name } else { typed };
        ui.label(
            egui::RichText::new(crate::i18n::fmt(
                tl!("{provider} needs an app password, not the account's password (two-step verification must be on): account {address} → Security → App passwords."),
                &[("provider", preset.name), ("address", account)],
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
        });
    });
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
