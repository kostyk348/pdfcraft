//! The opt-in network backend: a DeepLX-compatible HTTP endpoint (`POST <endpoint>` with
//! `{"text", "source_lang", "target_lang"}`, answering `{"code", "message", "data"}`).
//!
//! Nothing here runs unless a caller builds a [`Deeplx`], which takes the endpoint it must talk to:
//! there is no default endpoint, no discovery and no request at construction.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::{Error, Result, Translator};

/// Most bytes an answer may hold; the translation of one paragraph is a few hundred.
const MAX_RESPONSE_BYTES: u64 = 1 << 20;
/// The longest endpoint URL accepted.
const MAX_ENDPOINT: usize = 2048;
/// The shortest and longest timeout accepted, in seconds.
const MIN_TIMEOUT_SECS: f64 = 1.0;
const MAX_TIMEOUT_SECS: f64 = 300.0;
/// Most characters one request may carry, the limit a DeepL-compatible service applies.
const MAX_TEXT_CHARS: usize = 5000;

/// A DeepLX-compatible HTTP translator.
pub struct Deeplx {
    endpoint: String,
    source_lang: String,
    target_lang: String,
    agent: ureq::Agent,
    requests: usize,
    /// Answers already received, so a repeated sentence costs one request.
    cache: HashMap<String, String>,
}

impl Deeplx {
    /// A translator that will POST to `endpoint` when asked for a translation.
    ///
    /// The endpoint is required and must be an `http://` or `https://` URL. The language codes are
    /// passed through to the service (`"auto"` is what DeepL calls "detect the source").
    pub fn new(endpoint: &str, source_lang: &str, target_lang: &str, timeout_secs: f64) -> Result<Self> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT || endpoint.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::Invalid("network.endpoint must be one http(s) URL with no spaces".into()));
        }
        let uri: ureq::http::Uri = endpoint.parse().map_err(|_| Error::Invalid(format!("network.endpoint {endpoint:?} is not a URL")))?;
        match uri.scheme_str() {
            Some("http") | Some("https") => {}
            _ => return Err(Error::Invalid(format!("network.endpoint {endpoint:?} must start with http:// or https://"))),
        }
        if uri.authority().is_none() {
            return Err(Error::Invalid(format!("network.endpoint {endpoint:?} names no host")));
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            source_lang: language("source", source_lang)?,
            target_lang: language("target", target_lang)?,
            agent: agent(timeout_secs),
            requests: 0,
            cache: HashMap::new(),
        })
    }

    /// The endpoint this translator talks to (no request is made).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// A language code the service can be asked for: `EN`, `RU`, `ZH-HANS`, `auto`, …
fn language(which: &str, code: &str) -> Result<String> {
    let code = code.trim();
    if code.is_empty() || code.len() > 16 || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::Invalid(format!("network.{which} must be a language code such as EN, RU or auto")));
    }
    Ok(code.to_owned())
}

/// One HTTP client, with the status read by the caller so a refusal's body can be reported.
fn agent(timeout_secs: f64) -> ureq::Agent {
    let secs = if timeout_secs.is_finite() { timeout_secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS) } else { crate::DEFAULT_TIMEOUT_SECS };
    let timeout = Duration::from_secs_f64(secs);
    ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().new_agent()
}

/// The translation in an answer: DeepLX's `data`, a bare JSON string, or DeepL's own
/// `translations[0].text`.
fn translation(answer: &Value) -> Option<String> {
    match answer {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object.get("data").and_then(Value::as_str).map(str::to_owned).or_else(|| {
            object
                .get("translations")
                .and_then(Value::as_array)
                .and_then(|list| list.first())
                .and_then(|entry| entry.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        }),
        _ => None,
    }
}

impl Translator for Deeplx {
    fn translate(&mut self, text: &str) -> Result<Option<String>> {
        if let Some(hit) = self.cache.get(text) {
            return Ok(Some(hit.clone()));
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(Error::Invalid(format!(
                "a paragraph of {} characters is longer than one request may carry ({MAX_TEXT_CHARS}); split it or translate it offline",
                text.chars().count()
            )));
        }
        let body = serde_json::json!({ "text": text, "source_lang": self.source_lang, "target_lang": self.target_lang });
        let bytes = serde_json::to_vec(&body).map_err(|e| Error::Invalid(format!("the translation request could not be encoded ({e})")))?;
        self.requests = self.requests.saturating_add(1);
        let mut response = self
            .agent
            .post(self.endpoint.as_str())
            .content_type("application/json")
            .send(bytes)
            .map_err(|e| Error::Network(format!("{} could not be reached ({e})", self.endpoint)))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|e| Error::Network(format!("{} answered with no readable body ({e})", self.endpoint)))?;
        if !(200..300).contains(&status) {
            return Err(Error::Network(format!("{} answered HTTP {status}: {}", self.endpoint, crate::quote(&body))));
        }
        let answer: Value =
            serde_json::from_str(&body).map_err(|_| Error::Network(format!("{} did not answer JSON: {}", self.endpoint, crate::quote(&body))))?;
        if let Some(code) = answer.get("code").and_then(Value::as_i64)
            && code != 200
        {
            let message = answer.get("message").and_then(Value::as_str).unwrap_or("no message");
            return Err(Error::Network(format!("{} refused the translation (code {code}: {})", self.endpoint, crate::quote(message))));
        }
        let translated = translation(&answer)
            .ok_or_else(|| Error::Network(format!("{} answered without a translation: {}", self.endpoint, crate::quote(&body))))?;
        if translated.trim().is_empty() {
            return Err(Error::Network(format!("{} answered with an empty translation", self.endpoint)));
        }
        self.cache.insert(text.to_owned(), translated.clone());
        Ok(Some(translated))
    }

    fn requests(&self) -> usize {
        self.requests
    }
}
