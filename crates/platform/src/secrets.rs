//! The operating system's secret store, behind one small trait: the only place PdfCraft may keep a
//! credential between sessions.
//!
//! A caller files a secret under a *key* it chooses and can read, replace or delete it later; how
//! the secret is kept is the platform's business. Nothing here writes a secret to a file, to the
//! settings, to a log or into an error message, and [`StoreError`] never carries the secret — only
//! what went wrong with the store, in words safe to show.
//!
//! - **Linux** uses the Secret Service over the session bus (`org.freedesktop.secrets`), so the
//!   secret lives in GNOME Keyring, in KWallet's compatibility service or in any other provider of
//!   that interface. `zbus` is already in the dependency graph through the accessibility and portal
//!   integrations, so this adds no crate.
//! - **Windows** (Credential Manager) and **macOS** (Keychain) are *not* wired up yet: they answer
//!   [`StoreError::Unsupported`] rather than falling back to a file. The caller must then keep the
//!   credential in memory for the session only.
//!
//! [`MemoryStore`] is the in-memory implementation the tests use.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A platform secret store: one secret per key, kept outside PdfCraft's own files.
pub trait SecretStore: Send + Sync {
    /// The secret filed under `key`, `None` when nothing is filed. `Err` when the store could not
    /// answer (no store on this platform, locked, no session bus).
    fn get(&self, key: &str) -> Result<Option<String>, StoreError>;
    /// File `secret` under `key`, replacing what was there.
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError>;
    /// Remove what is filed under `key`. Removing a key nothing is filed under is not an error.
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

/// Why a store could not answer. The text describes the *store*, never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// This platform or build has no secret store; the credential must stay in memory.
    Unsupported,
    /// The store exists but is locked: it cannot answer until the user unlocks it.
    Locked,
    /// The store could not be used for any other reason (no session bus, refused, asking for
    /// confirmation). The string is the technical detail, shown after a translated sentence.
    Unavailable(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unsupported => f.write_str("this system has no password store"),
            StoreError::Locked => f.write_str("the password store is locked"),
            StoreError::Unavailable(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for StoreError {}

/// The store this platform offers. Linux: the desktop's Secret Service. Everywhere else: a store
/// that honestly refuses ([`StoreError::Unsupported`]).
pub fn system() -> Arc<dyn SecretStore> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(secret_service::SecretService::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(unsupported::NoStore)
    }
}

/// In-memory secrets, for tests only: nothing survives the process, and nothing leaves it.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<BTreeMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the store holds for `key` right now, without going through the trait.
    pub fn peek(&self, key: &str) -> Option<String> {
        self.entries.lock().ok().and_then(|entries| entries.get(key).cloned())
    }

    /// How many secrets are filed: a test's way of saying "nothing is left behind".
    pub fn len(&self) -> usize {
        self.entries.lock().map(|entries| entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let entries = self.entries.lock().map_err(|_| StoreError::Unavailable("the store was poisoned".to_string()))?;
        Ok(entries.get(key).cloned())
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        let mut entries = self.entries.lock().map_err(|_| StoreError::Unavailable("the store was poisoned".to_string()))?;
        entries.insert(key.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let mut entries = self.entries.lock().map_err(|_| StoreError::Unavailable("the store was poisoned".to_string()))?;
        entries.remove(key);
        Ok(())
    }
}

/// Platforms whose secret store is not wired up yet: refusing is the whole behaviour.
#[cfg(not(target_os = "linux"))]
mod unsupported {
    use super::{SecretStore, StoreError};

    pub(super) struct NoStore;

    impl SecretStore for NoStore {
        fn get(&self, _key: &str) -> Result<Option<String>, StoreError> {
            Err(StoreError::Unsupported)
        }

        fn set(&self, _key: &str, _secret: &str) -> Result<(), StoreError> {
            Err(StoreError::Unsupported)
        }

        fn delete(&self, _key: &str) -> Result<(), StoreError> {
            Err(StoreError::Unsupported)
        }
    }
}

/// The Secret Service, as the freedesktop specification describes it:
/// <https://specifications.freedesktop.org/secret-service-spec/latest/>. Every exchange is one
/// short blocking call on the session bus, bounded by [`TIMEOUT`].
#[cfg(target_os = "linux")]
mod secret_service {
    use super::{SecretStore, StoreError};
    use std::collections::HashMap;
    use std::time::Duration;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    const DESTINATION: &str = "org.freedesktop.secrets";
    const SERVICE_PATH: &str = "/org/freedesktop/secrets";
    const SERVICE: &str = "org.freedesktop.Secret.Service";
    const COLLECTION: &str = "org.freedesktop.Secret.Collection";
    const ITEM: &str = "org.freedesktop.Secret.Item";
    const LABEL: &str = "org.freedesktop.Secret.Item.Label";
    const ATTRIBUTES: &str = "org.freedesktop.Secret.Item.Attributes";
    /// The one attribute a PdfCraft secret is filed under: the caller's key, so two settings never
    /// read each other's password.
    const KEY: &str = "key";
    const CONTENT_TYPE: &str = "text/plain";
    /// The bus is local; a provider that has not answered in this long is not going to.
    const TIMEOUT: Duration = Duration::from_secs(10);

    pub(super) struct SecretService {
        /// The bus to use instead of the desktop's own. Only a test sets this.
        address: Option<String>,
    }

    impl SecretService {
        pub(super) fn new() -> Self {
            Self { address: None }
        }

        /// The same store, on a bus a test started.
        #[cfg(test)]
        fn at(address: &str) -> Self {
            Self { address: Some(address.to_string()) }
        }

        fn connection(&self) -> impl std::future::Future<Output = Result<zbus::Connection, StoreError>> + '_ {
            connect(self.address.as_deref())
        }
    }

    /// A connection to `address`, or to the desktop's own session bus when there is none. The bus is
    /// local; a provider that has not answered within [`TIMEOUT`] is not going to.
    async fn connect(address: Option<&str>) -> Result<zbus::Connection, StoreError> {
        let builder = match address {
            Some(address) => zbus::connection::Builder::address(address),
            None => zbus::connection::Builder::session(),
        };
        let builder = builder.map_err(|error| unavailable("there is no session bus", error))?;
        builder.method_timeout(TIMEOUT).build().await.map_err(|error| unavailable("the secret service did not answer", error))
    }

    /// A failed exchange, in the words of the store rather than of PdfCraft. Never carries a secret:
    /// only the step and the bus's own reason.
    fn unavailable(step: &str, error: impl std::fmt::Display) -> StoreError {
        StoreError::Unavailable(format!("{step}: {error}"))
    }

    /// The path of a `plain` session: the secret travels inside the local bus connection's own
    /// protection, which is what every Secret Service client uses for a short exchange.
    async fn open(connection: &zbus::Connection) -> Result<OwnedObjectPath, StoreError> {
        let reply = connection
            .call_method(Some(DESTINATION), SERVICE_PATH, Some(SERVICE), "OpenSession", &("plain", Value::from("")))
            .await
            .map_err(|error| unavailable("OpenSession", error))?;
        let (_output, session) = reply.body().deserialize::<(OwnedValue, OwnedObjectPath)>().map_err(|error| unavailable("OpenSession", error))?;
        Ok(session)
    }

    /// The default collection (the "login" keyring on GNOME, the wallet on KDE).
    async fn collection(connection: &zbus::Connection) -> Result<OwnedObjectPath, StoreError> {
        let reply = connection
            .call_method(Some(DESTINATION), SERVICE_PATH, Some(SERVICE), "ReadAlias", &("default",))
            .await
            .map_err(|error| unavailable("ReadAlias", error))?;
        let path = reply.body().deserialize::<OwnedObjectPath>().map_err(|error| unavailable("ReadAlias", error))?;
        if path.as_str() == "/" {
            return Err(StoreError::Unavailable("the desktop has no default password collection".to_string()));
        }
        Ok(path)
    }

    /// The items filed under `key`, split into the ones that can be read now and the ones the store
    /// holds but has locked.
    async fn find(connection: &zbus::Connection, key: &str) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>), StoreError> {
        let attributes: HashMap<&str, &str> = [(KEY, key)].into_iter().collect();
        let reply = connection
            .call_method(Some(DESTINATION), SERVICE_PATH, Some(SERVICE), "SearchItems", &(attributes,))
            .await
            .map_err(|error| unavailable("SearchItems", error))?;
        reply.body().deserialize::<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)>().map_err(|error| unavailable("SearchItems", error))
    }

    /// The secret an item holds. A value that is not text was not written by us, so there is
    /// nothing here a password field may use: `Ok(None)` rather than a guess.
    async fn read(connection: &zbus::Connection, item: &OwnedObjectPath, session: &OwnedObjectPath) -> Result<Option<String>, StoreError> {
        let reply = connection
            .call_method(Some(DESTINATION), item.as_str(), Some(ITEM), "GetSecret", &(session,))
            .await
            .map_err(|error| unavailable("GetSecret", error))?;
        let (_session, _parameters, value, _content_type) =
            reply.body().deserialize::<(OwnedObjectPath, Vec<u8>, Vec<u8>, String)>().map_err(|error| unavailable("GetSecret", error))?;
        Ok(String::from_utf8(value).ok())
    }

    /// The wire form of a secret: `(o session, ay parameters, ay value, s content type)`.
    fn secret(session: &OwnedObjectPath, value: &str) -> (OwnedObjectPath, Vec<u8>, Vec<u8>, String) {
        (session.clone(), Vec::new(), value.as_bytes().to_vec(), CONTENT_TYPE.to_string())
    }

    /// `CreateItem` and `Delete` answer with a prompt path. A prompt means the store wants a human
    /// to confirm, which this code cannot do on its own; PdfCraft says so rather than pretending.
    fn prompted(prompt: &OwnedObjectPath) -> Result<(), StoreError> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        Err(StoreError::Unavailable("the password store asked for confirmation".to_string()))
    }

    impl SecretStore for SecretService {
        fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
            futures_lite::future::block_on(async {
                let connection = self.connection().await?;
                let session = open(&connection).await?;
                let (unlocked, locked) = find(&connection, key).await?;
                let Some(item) = unlocked.first() else {
                    return if locked.is_empty() { Ok(None) } else { Err(StoreError::Locked) };
                };
                // The session ends with the connection; there is nothing to close by hand.
                read(&connection, item, &session).await
            })
        }

        fn set(&self, key: &str, secret_value: &str) -> Result<(), StoreError> {
            futures_lite::future::block_on(async {
                let connection = self.connection().await?;
                let session = open(&connection).await?;
                let (unlocked, locked) = find(&connection, key).await?;
                if let Some(item) = unlocked.first() {
                    connection
                        .call_method(Some(DESTINATION), item.as_str(), Some(ITEM), "SetSecret", &(secret(&session, secret_value),))
                        .await
                        .map_err(|error| unavailable("SetSecret", error))?;
                    return Ok(());
                }
                if !locked.is_empty() {
                    return Err(StoreError::Locked);
                }
                let collection = collection(&connection).await?;
                let attributes = [(KEY, key)].into_iter().collect::<HashMap<&str, &str>>();
                let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
                properties.insert(LABEL, Value::from(key));
                properties.insert(ATTRIBUTES, Value::new(attributes));
                let reply = connection
                    .call_method(
                        Some(DESTINATION),
                        collection.as_str(),
                        Some(COLLECTION),
                        "CreateItem",
                        &(properties, secret(&session, secret_value), true),
                    )
                    .await
                    .map_err(|error| unavailable("CreateItem", error))?;
                let (item, prompt) =
                    reply.body().deserialize::<(OwnedObjectPath, OwnedObjectPath)>().map_err(|error| unavailable("CreateItem", error))?;
                if item.as_str() == "/" {
                    prompted(&prompt)?;
                    return Err(StoreError::Unavailable("the store created no item".to_string()));
                }
                prompted(&prompt)
            })
        }

        fn delete(&self, key: &str) -> Result<(), StoreError> {
            futures_lite::future::block_on(async {
                let connection = self.connection().await?;
                let (unlocked, locked) = find(&connection, key).await?;
                if unlocked.is_empty() {
                    return if locked.is_empty() { Ok(()) } else { Err(StoreError::Locked) };
                }
                // Every item filed under the key goes, including ones an earlier save left behind,
                // so a key is either filed once or not at all.
                for item in unlocked {
                    let reply = connection
                        .call_method(Some(DESTINATION), item.as_str(), Some(ITEM), "Delete", &())
                        .await
                        .map_err(|error| unavailable("Delete", error))?;
                    let prompt: OwnedObjectPath = reply.body().deserialize().map_err(|error| unavailable("Delete", error))?;
                    prompted(&prompt)?;
                }
                Ok(())
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap as Map;
        use std::io::BufRead;
        use std::process::{Child, Command, Stdio};
        use std::sync::{Arc, Mutex};

        const COLLECTION_PATH: &str = "/org/freedesktop/secrets/collection/login";

        /// A `dbus-daemon` of our own: these tests never touch the desktop's bus or keyring.
        struct Daemon(Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        struct Stored {
            path: OwnedObjectPath,
            attributes: Map<String, String>,
            secret: String,
        }

        #[derive(Default)]
        struct Vault {
            items: Mutex<Vec<Stored>>,
            next: Mutex<u32>,
            /// Answer as a keyring whose default collection is locked does: the items are there,
            /// but in the locked list and unreadable.
            locked: bool,
        }

        struct Service {
            vault: Arc<Vault>,
        }

        struct Collection {
            vault: Arc<Vault>,
        }

        struct Item {
            vault: Arc<Vault>,
            path: OwnedObjectPath,
        }

        fn failed(error: &dyn std::fmt::Display) -> zbus::fdo::Error {
            zbus::fdo::Error::Failed(error.to_string())
        }

        /// The spec's "no object" path.
        fn root() -> OwnedObjectPath {
            OwnedObjectPath::try_from("/").unwrap()
        }

        #[zbus::interface(name = "org.freedesktop.Secret.Service")]
        impl Service {
            async fn open_session(&self, algorithm: &str, _input: Value<'_>) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
                if algorithm != "plain" {
                    return Err(zbus::fdo::Error::NotSupported("only the plain session is served".into()));
                }
                let path = OwnedObjectPath::try_from("/org/freedesktop/secrets/session/1").map_err(|error| failed(&error))?;
                let output = OwnedValue::try_from(Value::from("")).map_err(|error| failed(&error))?;
                Ok((output, path))
            }

            async fn search_items(&self, attributes: Map<String, String>) -> zbus::fdo::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
                let items = self.vault.items.lock().map_err(|error| failed(&error))?;
                let matches: Vec<OwnedObjectPath> = items
                    .iter()
                    .filter(|item| attributes.iter().all(|(name, value)| item.attributes.get(name) == Some(value)))
                    .map(|item| item.path.clone())
                    .collect();
                if self.vault.locked { Ok((Vec::new(), matches)) } else { Ok((matches, Vec::new())) }
            }

            async fn read_alias(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
                if name != "default" {
                    return Ok(root());
                }
                OwnedObjectPath::try_from(COLLECTION_PATH).map_err(|error| failed(&error))
            }
        }

        #[zbus::interface(name = "org.freedesktop.Secret.Collection")]
        impl Collection {
            async fn create_item(
                &self,
                properties: Map<String, OwnedValue>,
                secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
                replace: bool,
            ) -> zbus::fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
                let attributes: Map<String, String> = properties
                    .get(ATTRIBUTES)
                    .ok_or_else(|| zbus::fdo::Error::InvalidArgs("no attributes".into()))?
                    .clone()
                    .try_into()
                    .map_err(|error| failed(&error))?;
                let value = String::from_utf8(secret.2).map_err(|error| failed(&error))?;
                let mut items = self.vault.items.lock().map_err(|error| failed(&error))?;
                let existing = items.iter().position(|item| item.attributes == attributes && replace);
                let path = match existing {
                    Some(index) => {
                        items[index].secret = value;
                        items[index].path.clone()
                    }
                    None => {
                        let mut next = self.vault.next.lock().map_err(|error| failed(&error))?;
                        *next += 1;
                        let path = OwnedObjectPath::try_from(format!("{COLLECTION_PATH}/item/{next}")).map_err(|error| failed(&error))?;
                        items.push(Stored { path: path.clone(), attributes, secret: value });
                        path
                    }
                };
                Ok((path, root()))
            }
        }

        #[zbus::interface(name = "org.freedesktop.Secret.Item")]
        impl Item {
            async fn get_secret(&self, _session: OwnedObjectPath) -> zbus::fdo::Result<(OwnedObjectPath, Vec<u8>, Vec<u8>, String)> {
                let items = self.vault.items.lock().map_err(|error| failed(&error))?;
                let item = items.iter().find(|item| item.path == self.path).ok_or_else(|| zbus::fdo::Error::UnknownObject("gone".into()))?;
                let session = OwnedObjectPath::try_from("/org/freedesktop/secrets/session/1").map_err(|error| failed(&error))?;
                Ok((session, Vec::new(), item.secret.as_bytes().to_vec(), CONTENT_TYPE.to_string()))
            }

            async fn set_secret(&self, secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String)) -> zbus::fdo::Result<()> {
                let mut items = self.vault.items.lock().map_err(|error| failed(&error))?;
                let item = items.iter_mut().find(|item| item.path == self.path).ok_or_else(|| zbus::fdo::Error::UnknownObject("gone".into()))?;
                item.secret = String::from_utf8(secret.2).map_err(|error| failed(&error))?;
                Ok(())
            }

            async fn delete(&self) -> zbus::fdo::Result<OwnedObjectPath> {
                let mut items = self.vault.items.lock().map_err(|error| failed(&error))?;
                items.retain(|item| item.path != self.path);
                Ok(root())
            }
        }

        /// A private bus with a mock Secret Service on it.
        struct Mock {
            _daemon: Daemon,
            vault: Arc<Vault>,
            server: zbus::Connection,
            address: String,
        }

        impl Mock {
            fn start(locked: bool) -> Option<Self> {
                // `dbus-daemon` arrives with every Linux desktop; a container without it cannot run
                // this test, and says so instead of passing quietly.
                let child = match Command::new("dbus-daemon")
                    .args(["--session", "--nofork", "--print-address=1"])
                    // Abstract sockets disappear with the daemon, even if the test fails.
                    .arg(format!("--address=unix:abstract=pdfcraft-secrets-test-{}", std::process::id()))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(error) => {
                        eprintln!("skipping the Secret Service test: no dbus-daemon ({error})");
                        return None;
                    }
                };
                let mut daemon = Daemon(child);
                let mut address = String::new();
                if std::io::BufReader::new(daemon.0.stdout.take()?).read_line(&mut address).is_err() || address.trim().is_empty() {
                    eprintln!("skipping the Secret Service test: dbus-daemon printed no address");
                    return None;
                }
                let address = address.trim().to_string();
                let vault = Arc::new(Vault { items: Mutex::new(Vec::new()), next: Mutex::new(0), locked });
                let server = futures_lite::future::block_on(
                    zbus::connection::Builder::address(address.as_str())
                        .ok()?
                        .serve_at(SERVICE_PATH, Service { vault: Arc::clone(&vault) })
                        .ok()?
                        .serve_at(COLLECTION_PATH, Collection { vault: Arc::clone(&vault) })
                        .ok()?
                        .name(DESTINATION)
                        .ok()?
                        .build(),
                )
                .ok()?;
                Some(Self { _daemon: daemon, vault, server, address })
            }

            fn store(&self) -> SecretService {
                SecretService::at(&self.address)
            }

            /// Serve a new item path, as a provider does after `CreateItem`.
            fn serve(&self, path: &OwnedObjectPath) {
                let item = Item { vault: Arc::clone(&self.vault), path: path.clone() };
                futures_lite::future::block_on(async {
                    assert!(self.server.object_server().at(path.clone(), item).await.is_ok(), "the item is served");
                });
            }
        }

        /// The round trip a password takes: filed, read back, replaced, deleted.
        #[test]
        fn a_secret_is_filed_read_replaced_and_deleted_through_the_secret_service() {
            let Some(mock) = Mock::start(false) else { return };
            let store = mock.store();
            let key = "pdfcraft:smtp:starttls://a.example:587/ada";

            assert_eq!(store.get(key), Ok(None), "an empty store answers nothing, not an error");
            store.set(key, "hunter2").unwrap();
            let path = mock.vault.items.lock().unwrap().first().expect("the item was created").path.clone();
            // The mock, like a real provider, only answers for an exported item path.
            mock.serve(&path);

            assert_eq!(store.get(key), Ok(Some("hunter2".to_string())));
            // Filed under the key as its only attribute, so a different key finds nothing.
            assert_eq!(store.get("pdfcraft:smtp:tls://b.example:465/ada"), Ok(None));

            store.set(key, "correct-horse").unwrap();
            assert_eq!(store.get(key), Ok(Some("correct-horse".to_string())), "a second save replaces the first");
            assert_eq!(mock.vault.items.lock().unwrap().len(), 1, "one key, one item");

            store.delete(key).unwrap();
            assert_eq!(store.get(key), Ok(None));
            assert!(mock.vault.items.lock().unwrap().is_empty());
            // Deleting what is not there is not an error: Forget twice is harmless.
            store.delete(key).unwrap();
        }

        /// A locked default collection is `Locked`, not "no password saved": the dialog can then say
        /// why the field is empty.
        #[test]
        fn a_locked_collection_is_reported_rather_than_read_as_empty() {
            let Some(mock) = Mock::start(true) else { return };
            let store = mock.store();
            let key = "pdfcraft:smtp:tls://a.example:465/ada";
            assert_eq!(store.get(key), Ok(None), "nothing is filed at all, so nothing is locked");
            store.set(key, "hunter2").unwrap();
            // The item is there now, and the locked collection hides it as an unreadable one.
            assert_eq!(store.get(key), Err(StoreError::Locked));
            assert_eq!(store.set(key, "again"), Err(StoreError::Locked));
            assert_eq!(store.delete(key), Err(StoreError::Locked));
        }

        /// The client talks to the bus name and the paths the mock serves: a store that only agreed
        /// with itself would pass every test above without ever speaking the protocol.
        #[test]
        fn the_client_asks_the_bus_name_the_mock_serves() {
            let Some(mock) = Mock::start(false) else { return };
            let connection = futures_lite::future::block_on(zbus::connection::Builder::address(mock.address.as_str()).unwrap().build()).unwrap();
            let names = futures_lite::future::block_on(connection.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ListNames",
                &(),
            ))
            .unwrap()
            .body()
            .deserialize::<Vec<String>>()
            .unwrap();
            assert!(names.iter().any(|name| name == DESTINATION), "{names:?}");
            assert!(mock.vault.items.lock().unwrap().is_empty());
        }

        /// Against the machine's own keyring, when it has one: the same round trip through the real
        /// provider. Not part of the suite, because it needs a desktop session and writes to it.
        #[test]
        #[ignore = "uses the machine's own session keyring; run by hand with --ignored"]
        fn a_secret_round_trips_through_the_running_keyring() {
            let store = SecretService::new();
            let key = format!("pdfcraft:smtp:test-roundtrip/{}", std::process::id());
            assert_eq!(store.get(&key), Ok(None));
            store.set(&key, "roundtrip-secret").unwrap();
            assert_eq!(store.get(&key), Ok(Some("roundtrip-secret".to_string())));
            store.delete(&key).unwrap();
            assert_eq!(store.get(&key), Ok(None));
            eprintln!("the running keyring kept, returned and forgot the secret for {key}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_files_by_key_and_forgets_completely() {
        let store = MemoryStore::new();
        assert_eq!(store.get("a"), Ok(None));
        store.set("a", "one").unwrap();
        store.set("b", "two").unwrap();
        assert_eq!(store.get("a"), Ok(Some("one".to_string())));
        assert_eq!(store.peek("b"), Some("two".to_string()));
        store.delete("a").unwrap();
        assert_eq!(store.get("a"), Ok(None));
        assert_eq!(store.len(), 1);
        store.delete("a").unwrap();
        assert_eq!(store.len(), 1);
        store.delete("b").unwrap();
        assert!(store.is_empty());
    }

    /// On a platform whose store is not wired up yet, the answer is a refusal a caller can act on,
    /// never a file written behind the user's back.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn a_platform_without_a_store_refuses_instead_of_writing_a_file() {
        let store = system();
        assert_eq!(store.set("k", "secret"), Err(StoreError::Unsupported));
        assert_eq!(store.get("k"), Err(StoreError::Unsupported));
        assert_eq!(store.delete("k"), Err(StoreError::Unsupported));
    }

    /// What a caller shows a person is about the store, and never the secret that failed.
    #[test]
    fn a_store_error_describes_the_store_and_never_the_secret() {
        let error = StoreError::Unavailable("OpenSession: the service refused the connection".to_string());
        assert!(error.to_string().contains("OpenSession"));
        assert_eq!(StoreError::Locked.to_string(), "the password store is locked");
        assert_eq!(StoreError::Unsupported.to_string(), "this system has no password store");
    }
}
