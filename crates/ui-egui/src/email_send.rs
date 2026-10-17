//! File ▸ Send as email…: the opt-in step that delivers a document as a message, and the only
//! place the interface opens a socket.
//!
//! The message is the one File ▸ Save as email message… writes — the same engine call
//! (`Session::email_message`), so what a receiver keeps is byte for byte the `.eml` — and all this
//! adds is the SMTP session `doc_email_send` performs (AGENTS.md §3: everything the interface can do
//! is reachable headlessly, and both go through the same code).
//!
//! **Nothing connects until the user presses Send.** Opening the dialog, filling it in and
//! cancelling it opens no socket at all; the session itself runs off the frame thread, so a relay
//! that stalls cannot freeze the window. Credentials live in the draft for this session only: they
//! are masked in the field, never written to the settings, never logged (the SMTP client redacts the
//! `AUTH` token from its session log) and never put in a notice.

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
        text.parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| tl!("The port is a number from 1 to 65535").to_string())
    }
}

/// What one session did, kept for the notice and for the tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The relay took the message for these recipients (and refused the ones listed, if any).
    Delivered {
        host: String,
        port: u16,
        accepted: Vec<String>,
        refused: Vec<(String, String)>,
        bytes: usize,
        reply: String,
    },
    /// Nothing was delivered, and this says why in words a person can act on.
    Failed(String),
}

/// The dialog's state and the session it started.
#[derive(Default)]
pub struct EmailSend {
    pub draft: Draft,
    /// The last session's outcome (`None` before the first one).
    pub outcome: Option<Outcome>,
    /// The session running off the frame thread.
    running: Option<std::sync::mpsc::Receiver<Outcome>>,
}

impl PdfCraftApp {
    /// File ▸ Send as email…: open the dialog. It opens no socket by itself; the relay is only
    /// contacted by the Send button (see [`Self::send_email`]).
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
        let host = draft.host.trim().to_string();
        if host.is_empty() {
            return Err(tl!("The server needs a host name").to_string());
        }
        if draft.transport == Transport::None && (!draft.username.trim().is_empty() || !draft.password.is_empty()) {
            return Err(tl!("A password needs an encrypted connection: choose STARTTLS or TLS").to_string());
        }
        let port = draft.port()?;
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

        let (username, password) = if draft.auth && !draft.username.trim().is_empty() {
            (Some(draft.username.clone()), Some(draft.password.clone()))
        } else {
            (None, None)
        };
        let server = pdfcraft_smtp::Server {
            host,
            port,
            security: draft.transport.client(),
            username,
            password,
            timeout: std::time::Duration::from_secs(30),
            client_name: "pdfcraft.local".to_string(),
            crlf: false,
            extra_roots: Vec::new(),
        };
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
}

/// The dialog's fields. Returns `(send, cancel)`; the caller runs the send, so a reason to stay open
/// can be shown without leaving the dialog.
pub(crate) fn body(ui: &mut egui::Ui, draft: &mut Draft, t: &Tokens) -> (bool, bool) {
    ui.label(egui::RichText::new(tl!("Send as email")).font(theme::semibold(18.0)));
    ui.add_space(6.0);
    ui.label(egui::RichText::new(tl!("The message is the one Save as email message… writes, delivered over SMTP. Nothing is sent until you press Send.")).size(12.0).color(t.text_muted));
    ui.add_space(8.0);
    egui::Grid::new("email-send").num_columns(2).spacing([12.0, 8.0]).min_col_width(88.0).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, label: &str, value: &mut String, width: f32| {
            let l = ui.label(tl!(label));
            ui.add(egui::TextEdit::singleline(value).desired_width(width)).labelled_by(l.id);
            ui.end_row();
        };
        row(ui, "Server:", &mut draft.host, 300.0);
        let l = ui.label(tl!("Port:"));
        ui.add(egui::TextEdit::singleline(&mut draft.port).desired_width(80.0).hint_text(draft.transport.port().to_string())).labelled_by(l.id);
        ui.end_row();
        ui.label(tl!("Security:"));
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
        ui.end_row();
        row(ui, "From:", &mut draft.from, 300.0);
        row(ui, "To:", &mut draft.to, 300.0);
        row(ui, "Cc:", &mut draft.cc, 300.0);
        row(ui, "Subject:", &mut draft.subject, 300.0);
    });
    if draft.transport == Transport::None {
        ui.label(egui::RichText::new(tl!("Not encrypted: the message travels in the clear")).size(12.0).color(t.text_muted));
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
    let mut send = false;
    let mut cancel = false;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if widgets::pill_button(ui, tl!("Send"), true).clicked() {
            send = true;
        }
        if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
            cancel = true;
        }
    });
    (send, cancel)
}
