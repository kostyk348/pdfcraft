//! Translation backends that talk to a service over HTTP.
//!
//! Two services are supported, because they answer different questions:
//!
//! - **DeepLX**: a local server (default `http://127.0.0.1:1188`) that speaks the DeepL request
//!   shape. Local means no key and no rate limit shared with strangers, which is what a batch of
//!   datasheets needs.
//! - **Google Translate**: the public `translate_a/single` endpoint (no key, rate limited and
//!   best-effort) and the official v2 REST API when the caller has a key. The two share a request
//!   builder and differ only in where the text travels and how the answer is shaped.
//!
//! No backend here logs or echoes an API key, and none of them is called by a test: the response
//! parsers are tested against canned answers, so the test suite stays offline.

use std::time::Duration;

use serde_json::Value;

use crate::Translator;

/// A short timeout: a translation request that hangs is worse than one that fails, because the
/// caller is usually a batch job holding a document open.
const TIMEOUT: Duration = Duration::from_secs(20);

/// How much of an answer is read. A service that returns megabytes is not answering a translation
/// request, and the document waits for it.
const BODY_LIMIT: u64 = 4 << 20;

fn agent() -> Result<ureq::Agent, String> {
    let roots = os_roots()?;
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
        .build()
        .new_agent())
}

/// The certificate authorities the operating system trusts.
fn os_roots() -> Result<ureq::tls::RootCerts, String> {
    let found = rustls_native_certs::load_native_certs();
    let certs: Vec<ureq::tls::Certificate<'static>> =
        found.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    if certs.is_empty() {
        return Err("no trusted certificates found on this system".into());
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

/// Percent-encode a query value. Space becomes `%20`, as both services expect.
pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn read_body(response: ureq::Body) -> Result<String, String> {
    let mut body = response;
    body.with_config().limit(BODY_LIMIT).read_to_string().map_err(|e| format!("unreadable answer ({e})"))
}

/// A backend that promises one answer per input, whatever the service does with batches.
trait PerSegment {
    fn one(&mut self, text: &str) -> Result<String, String>;
}

impl<T: PerSegment + Named> Translator for T {
    fn id(&self) -> String {
        self.name().to_string()
    }

    fn translate(&mut self, input: &[String]) -> Result<Vec<String>, String> {
        input.iter().map(|text| self.one(text)).collect()
    }
}

trait Named {
    fn name(&self) -> &'static str;
}

/// DeepLX: `POST /translate` with `{"text", "source_lang", "target_lang"}` answers
/// `{"code": 200, "data": "…"}`.
pub struct DeepLx {
    pub endpoint: String,
    pub source: String,
    pub target: String,
    agent: Option<ureq::Agent>,
}

impl DeepLx {
    pub fn new(endpoint: impl Into<String>, source: impl Into<String>, target: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into(), source: source.into(), target: target.into(), agent: None }
    }

    /// The usual local DeepLX install.
    pub fn local(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self::new("http://127.0.0.1:1188/translate", source, target)
    }

    fn agent(&mut self) -> Result<&ureq::Agent, String> {
        if self.agent.is_none() {
            self.agent = Some(agent()?);
        }
        self.agent.as_ref().ok_or_else(|| "no http agent".to_string())
    }
}

impl Named for DeepLx {
    fn name(&self) -> &'static str {
        "deeplx"
    }
}

impl PerSegment for DeepLx {
    fn one(&mut self, text: &str) -> Result<String, String> {
        let payload = serde_json::json!({
            "text": text,
            "source_lang": self.source.to_uppercase(),
            "target_lang": self.target.to_uppercase(),
        })
        .to_string();
        let endpoint = self.endpoint.clone();
        let response = self
            .agent()?
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .send(payload.as_str())
            .map_err(|e| format!("DeepLX request to {endpoint} failed ({e}); is the server running?"))?;
        let status = response.status().as_u16();
        let body = read_body(response.into_body())?;
        if !(200..300).contains(&status) {
            return Err(format!("DeepLX answered http {status}"));
        }
        parse_deeplx(&body)
    }
}

/// The DeepLX answer, as a function of the body text alone so it can be tested offline.
pub fn parse_deeplx(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("DeepLX answer is not JSON ({e})"))?;
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 200 {
            let message = value.get("message").and_then(Value::as_str).unwrap_or("no message");
            return Err(format!("DeepLX refused the request (code {code}: {message})"));
        }
    }
    value
        .get("data")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "DeepLX answer has no data field".to_string())
}

/// Google Translate, public endpoint: `translate_a/single` answers a ragged array whose first
/// element is the list of translated chunks.
pub struct GoogleFree {
    pub source: String,
    pub target: String,
    agent: Option<ureq::Agent>,
}

impl GoogleFree {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self { source: source.into(), target: target.into(), agent: None }
    }

    fn agent(&mut self) -> Result<&ureq::Agent, String> {
        if self.agent.is_none() {
            self.agent = Some(agent()?);
        }
        self.agent.as_ref().ok_or_else(|| "no http agent".to_string())
    }
}

impl Named for GoogleFree {
    fn name(&self) -> &'static str {
        "google"
    }
}

impl PerSegment for GoogleFree {
    fn one(&mut self, text: &str) -> Result<String, String> {
        let url = format!(
            "https://translate.googleapis.com/translate_a/single?client=gtx&dt=t&sl={}&tl={}&q={}",
            urlencode(&self.source),
            urlencode(&self.target),
            urlencode(text)
        );
        let response = self
            .agent()?
            .get(&url)
            .header("User-Agent", concat!("PdfCraft/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|e| format!("Google Translate request failed ({e})"))?;
        let status = response.status().as_u16();
        let body = read_body(response.into_body())?;
        if !(200..300).contains(&status) {
            return Err(format!("Google Translate answered http {status}"));
        }
        parse_google_free(&body)
    }
}

/// The public endpoint's answer: `[[["перевод","source",…],…],…]`. Every chunk of the first
/// element is a translated piece of the input, in order.
pub fn parse_google_free(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("Google answer is not JSON ({e})"))?;
    let chunks = value.get(0).and_then(Value::as_array).ok_or_else(|| "Google answer has no chunks".to_string())?;
    let mut out = String::new();
    for chunk in chunks {
        if let Some(piece) = chunk.get(0).and_then(Value::as_str) {
            out.push_str(piece);
        }
    }
    if out.is_empty() {
        return Err("Google returned no text".into());
    }
    Ok(out)
}

/// Google Translate, official v2 REST API. The key comes from the caller (normally
/// `GOOGLE_TRANSLATE_API_KEY`), is only ever put in the URL of a request, and is never returned or
/// logged: an error from this backend names the status, not the request.
pub struct GoogleApi {
    pub key: String,
    pub source: String,
    pub target: String,
    agent: Option<ureq::Agent>,
}

impl GoogleApi {
    pub fn new(key: impl Into<String>, source: impl Into<String>, target: impl Into<String>) -> Self {
        Self { key: key.into(), source: source.into(), target: target.into(), agent: None }
    }

    /// The key from `GOOGLE_TRANSLATE_API_KEY`, if the environment has one.
    pub fn from_env(source: impl Into<String>, target: impl Into<String>) -> Option<Self> {
        std::env::var("GOOGLE_TRANSLATE_API_KEY").ok().filter(|k| !k.trim().is_empty()).map(|key| Self::new(key, source, target))
    }

    /// Never the key: what a caller may print.
    pub fn id_masked(&self) -> String {
        "google-api".to_string()
    }

    fn agent(&mut self) -> Result<&ureq::Agent, String> {
        if self.agent.is_none() {
            self.agent = Some(agent()?);
        }
        self.agent.as_ref().ok_or_else(|| "no http agent".to_string())
    }
}

impl Named for GoogleApi {
    fn name(&self) -> &'static str {
        "google-api"
    }
}

impl PerSegment for GoogleApi {
    fn one(&mut self, text: &str) -> Result<String, String> {
        let url = format!("https://translation.googleapis.com/language/translate/v2?key={}", urlencode(&self.key));
        let form = format!(
            "q={}&target={}&source={}&format=text",
            urlencode(text),
            urlencode(&self.target),
            urlencode(&self.source)
        );
        let response = self
            .agent()?
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(form.as_str())
            .map_err(|e| format!("Google API request failed ({e})"))?;
        let status = response.status().as_u16();
        let body = read_body(response.into_body())?;
        if !(200..300).contains(&status) {
            return Err(format!("Google API answered http {status}"));
        }
        parse_google_api(&body)
    }
}

/// The official API's answer: `{"data": {"translations": [{"translatedText": "…"}]}}`.
pub fn parse_google_api(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("Google API answer is not JSON ({e})"))?;
    if let Some(error) = value.get("error") {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("no message");
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        return Err(format!("Google API refused the request (code {code}: {message})"));
    }
    let text = value
        .get("data")
        .and_then(|d| d.get("translations"))
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .and_then(|t| t.get("translatedText"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Google API answer has no translation".to_string())?;
    Ok(text.to_string())
}

/// The backend a run asked for, with the language pair applied.
pub fn by_name(
    name: &str,
    source: &str,
    target: &str,
    endpoint: Option<&str>,
) -> Result<Box<dyn Translator>, String> {
    match name {
        "deeplx" => Ok(Box::new(match endpoint {
            Some(url) => DeepLx::new(url, source, target),
            None => DeepLx::local(source, target),
        })),
        "google" => Ok(Box::new(GoogleFree::new(source, target))),
        "google-api" => match GoogleApi::from_env(source, target) {
            Some(api) => Ok(Box::new(api)),
            None => Err("google-api needs GOOGLE_TRANSLATE_API_KEY in the environment".into()),
        },
        "upper" => Ok(Box::new(crate::UpperCase)),
        other => Err(format!("unknown backend {other:?}: use deeplx, google, google-api or upper")),
    }
}
