use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{error, warn};

use crate::types::ChatMessage;

const MAX_ENCODED_ID_LEN: usize = 200;
const MAX_LONG_PREFIX_LEN: usize = 120;

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Persisted representation of a conversation session stored on disk as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSession {
    pub session_id: String,
    pub model: Option<String>,
    pub updated_at_unix: u64,
    pub messages: Vec<ChatMessage>,
}

impl PersistedSession {
    fn new_empty(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            model: None,
            updated_at_unix: 0,
            messages: Vec::new(),
        }
    }
}

/// Lightweight metadata summary returned by `list_sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub model: Option<String>,
    pub message_count: usize,
    pub updated_at_unix: u64,
}

/// Errors arising from persistent session storage operations.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("invalid session_id: {0}")]
    InvalidSessionId(String),

    #[error("corrupted session file for '{session_id}' at '{path}': {source}")]
    CorruptedSession {
        session_id: String,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("session I/O error at '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize session '{session_id}': {source}")]
    Serialization {
        session_id: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug)]
enum SessionState {
    Unloaded,
    Loaded(Option<PersistedSession>),
}

/// Per-session cache slot synchronized by an individual `Mutex`.
#[derive(Debug)]
pub struct SessionSlot {
    state: SessionState,
}

/// Thread-safe, lazily-loaded, disk-persisted store for multi-turn chat sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    max_history_messages: usize,
    entries: Arc<Mutex<HashMap<String, Arc<Mutex<SessionSlot>>>>>,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf, max_history_messages: usize) -> Self {
        let clamped_max = match max_history_messages {
            0 => 1,
            n => n,
        };
        Self {
            sessions_dir,
            max_history_messages: clamped_max,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn max_history_messages(&self) -> usize {
        self.max_history_messages
    }

    /// Acquire the per-session mutex slot without holding the global map lock across I/O.
    pub async fn acquire_slot(
        &self,
        session_id: &str,
    ) -> Result<Arc<Mutex<SessionSlot>>, SessionStoreError> {
        let _ = encode_session_id(session_id)?;
        let mut map = self.entries.lock().await;
        match map.get(session_id) {
            Some(existing) => Ok(Arc::clone(existing)),
            None => {
                let slot = Arc::new(Mutex::new(SessionSlot {
                    state: SessionState::Unloaded,
                }));
                map.insert(session_id.to_string(), Arc::clone(&slot));
                Ok(slot)
            }
        }
    }

    /// Ensure the session slot is loaded from disk (or reset if requested) and return a mutable handle.
    pub async fn load_or_reset<'a>(
        &self,
        session_id: &str,
        slot: &'a mut SessionSlot,
        reset: bool,
    ) -> Result<&'a mut PersistedSession, SessionStoreError> {
        match reset {
            true => {
                self.remove_session_file(session_id).await?;
                slot.state = SessionState::Loaded(Some(PersistedSession::new_empty(session_id)));
            }
            false => {
                match slot.state {
                    SessionState::Unloaded => {
                        let loaded = self.read_session_from_disk(session_id).await?;
                        slot.state = SessionState::Loaded(loaded);
                    }
                    SessionState::Loaded(_) => {}
                }

                match slot.state {
                    SessionState::Loaded(None) => {
                        slot.state =
                            SessionState::Loaded(Some(PersistedSession::new_empty(session_id)));
                    }
                    SessionState::Loaded(Some(_)) => {}
                    SessionState::Unloaded => {}
                }
            }
        }

        match slot.state {
            SessionState::Loaded(Some(ref mut session)) => Ok(session),
            _ => unreachable!("session state must be Loaded(Some(_)) after initialization"),
        }
    }

    /// Collapse an empty unpersisted session back to `Loaded(None)` after an API rollback.
    pub fn cleanup_if_empty(&self, slot: &mut SessionSlot) {
        match slot.state {
            SessionState::Loaded(Some(ref session))
                if session.messages.is_empty() && session.updated_at_unix == 0 =>
            {
                slot.state = SessionState::Loaded(None);
            }
            _ => {}
        }
    }

    /// Prune messages to `max_history_messages`, update metadata, and atomically persist to disk.
    pub async fn save_session(
        &self,
        session: &mut PersistedSession,
        model: &str,
    ) -> Result<(), SessionStoreError> {
        prune_history(&mut session.messages, self.max_history_messages);
        session.model = Some(model.to_string());
        session.updated_at_unix = unix_timestamp_secs();
        self.write_session_atomically(session).await
    }

    /// Remove a session from both in-memory cache and disk storage.
    /// Returns `Ok(Some(discarded_count))` if the session existed, or `Ok(None)` if inactive.
    pub async fn reset_session(
        &self,
        session_id: &str,
    ) -> Result<Option<usize>, SessionStoreError> {
        let slot_arc = self.acquire_slot(session_id).await?;
        let mut slot = slot_arc.lock().await;

        let previous_count = match slot.state {
            SessionState::Loaded(Some(ref s)) => Some(s.messages.len()),
            SessionState::Loaded(None) => None,
            SessionState::Unloaded => match self.read_session_from_disk(session_id).await {
                Ok(Some(s)) => Some(s.messages.len()),
                Ok(None) => None,
                Err(SessionStoreError::CorruptedSession { path, source, .. }) => {
                    warn!(
                        "Removing corrupted session file '{}' during reset: {source}",
                        path.display()
                    );
                    Some(0)
                }
                Err(e) => return Err(e),
            },
        };

        self.remove_session_file(session_id).await?;
        slot.state = SessionState::Loaded(None);
        Ok(previous_count)
    }

    /// Retrieve a persisted session by ID (loading lazily from disk if needed).
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedSession>, SessionStoreError> {
        let slot_arc = self.acquire_slot(session_id).await?;
        let mut slot = slot_arc.lock().await;

        match slot.state {
            SessionState::Unloaded => {
                let loaded = self.read_session_from_disk(session_id).await?;
                slot.state = SessionState::Loaded(loaded);
            }
            SessionState::Loaded(_) => {}
        }

        match slot.state {
            SessionState::Loaded(ref opt) => Ok(opt.clone()),
            SessionState::Unloaded => unreachable!("slot state was just loaded"),
        }
    }

    /// List all persisted sessions in `sessions_dir`, sorted by `updated_at_unix` descending.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut read_dir = match tokio::fs::read_dir(&self.sessions_dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(SessionStoreError::Io {
                    path: self.sessions_dir.clone(),
                    source: e,
                });
            }
        };

        let mut summaries = Vec::new();
        loop {
            let entry = match read_dir.next_entry().await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(e) => {
                    return Err(SessionStoreError::Io {
                        path: self.sessions_dir.clone(),
                        source: e,
                    });
                }
            };

            let path = entry.path();
            let is_json_file = match path.extension().and_then(|ext| ext.to_str()) {
                Some("json") => match entry.file_type().await {
                    Ok(ft) => ft.is_file(),
                    Err(_) => false,
                },
                _ => false,
            };

            match is_json_file {
                false => continue,
                true => {}
            }

            let raw = match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(e) => {
                    warn!("Skipping unreadable session file '{}': {e}", path.display());
                    continue;
                }
            };

            match serde_json::from_str::<PersistedSession>(&raw) {
                Ok(session) => {
                    summaries.push(SessionSummary {
                        session_id: session.session_id,
                        model: session.model,
                        message_count: session.messages.len(),
                        updated_at_unix: session.updated_at_unix,
                    });
                }
                Err(e) => {
                    error!("Skipping corrupted session file '{}': {e}", path.display());
                }
            }
        }

        summaries.sort_by(|a, b| {
            b.updated_at_unix
                .cmp(&a.updated_at_unix)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        Ok(summaries)
    }

    async fn read_session_from_disk(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedSession>, SessionStoreError> {
        let path = self.session_file_path(session_id)?;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(SessionStoreError::Io { path, source: e });
            }
        };

        match serde_json::from_str::<PersistedSession>(&content) {
            Ok(session) => Ok(Some(session)),
            Err(source) => {
                error!(
                    "Failed to parse persisted session '{}' from '{}': {source}",
                    session_id,
                    path.display()
                );
                Err(SessionStoreError::CorruptedSession {
                    session_id: session_id.to_string(),
                    path,
                    source,
                })
            }
        }
    }

    async fn write_session_atomically(
        &self,
        session: &PersistedSession,
    ) -> Result<(), SessionStoreError> {
        self.ensure_sessions_dir().await?;
        let final_path = self.session_file_path(&session.session_id)?;
        let encoded = encode_session_id(&session.session_id)?;
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!("{encoded}.json.tmp.{}.{seq}", std::process::id());
        let tmp_path = self.sessions_dir.join(tmp_name);

        let payload = serde_json::to_vec_pretty(session).map_err(|source| {
            SessionStoreError::Serialization {
                session_id: session.session_id.clone(),
                source,
            }
        })?;

        let write_result = Self::write_temp_file(&tmp_path, &payload).await;
        match write_result {
            Ok(()) => {}
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(SessionStoreError::Io {
                    path: tmp_path,
                    source: e,
                });
            }
        }

        match tokio::fs::rename(&tmp_path, &final_path).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                Err(SessionStoreError::Io {
                    path: final_path,
                    source: e,
                })
            }
        }
    }

    async fn write_temp_file(path: &Path, payload: &[u8]) -> Result<(), std::io::Error> {
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
        }

        let mut file = opts.open(path).await?;
        file.write_all(payload).await?;
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn ensure_sessions_dir(&self) -> Result<(), SessionStoreError> {
        tokio::fs::create_dir_all(&self.sessions_dir)
            .await
            .map_err(|source| SessionStoreError::Io {
                path: self.sessions_dir.clone(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            tokio::fs::set_permissions(&self.sessions_dir, perms)
                .await
                .map_err(|source| SessionStoreError::Io {
                    path: self.sessions_dir.clone(),
                    source,
                })?;
        }

        Ok(())
    }

    async fn remove_session_file(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let path = self.session_file_path(session_id)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SessionStoreError::Io { path, source: e }),
        }
    }

    fn session_file_path(&self, session_id: &str) -> Result<PathBuf, SessionStoreError> {
        let encoded = encode_session_id(session_id)?;
        let file_name = format!("{encoded}.json");
        let rel = Path::new(&file_name);
        let mut components = rel.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Ok(self.sessions_dir.join(rel)),
            _ => Err(SessionStoreError::InvalidSessionId(
                "encoded filename failed path safety check".to_string(),
            )),
        }
    }
}

/// Encode an arbitrary `session_id` into a filesystem-safe, collision-free stem.
/// Characters outside `[A-Za-z0-9_-]` are percent-encoded as uppercase `%XX`.
/// Overlong encoded IDs are deterministically hashed with 128-bit FNV-1a while keeping a safe prefix.
pub fn encode_session_id(session_id: &str) -> Result<String, SessionStoreError> {
    match session_id.trim().is_empty() {
        true => {
            return Err(SessionStoreError::InvalidSessionId(
                "session_id cannot be empty".to_string(),
            ));
        }
        false => {}
    }

    let mut encoded = String::with_capacity(session_id.len());
    for byte in session_id.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => {
                encoded.push(*byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(hex_upper((byte >> 4) & 0x0F));
                encoded.push(hex_upper(byte & 0x0F));
            }
        }
    }

    match encoded.len() > MAX_ENCODED_ID_LEN {
        true => {
            let digest = fnv1a_128(session_id.as_bytes());
            let mut prefix_len = MAX_LONG_PREFIX_LEN.min(encoded.len());
            let bytes = encoded.as_bytes();
            prefix_len = match (prefix_len >= 1, prefix_len >= 2) {
                (true, _) if bytes[prefix_len - 1] == b'%' => prefix_len - 1,
                (_, true) if bytes[prefix_len - 2] == b'%' => prefix_len - 2,
                _ => prefix_len,
            };
            let prefix = &encoded[..prefix_len];
            Ok(format!("{prefix}~{digest:032x}"))
        }
        false => Ok(encoded),
    }
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => '0',
    }
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
    const FNV_OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn unix_timestamp_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => 0,
    }
}

/// Prune history while guaranteeing valid user/assistant alternation
/// and preserving any leading system prompt.
pub fn prune_history(history: &mut Vec<ChatMessage>, max_messages: usize) {
    let non_system_start = match !history.is_empty() && history[0].role == "system" {
        true => 1,
        false => 0,
    };

    while history.len() > max_messages && history.len() > non_system_start {
        history.remove(non_system_start);
    }

    // Guarantee that after the optional system prompt, the history starts with a user message
    while history.len() > non_system_start && history[non_system_start].role != "user" {
        history.remove(non_system_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agentrouter-session-store-test-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn test_encode_session_id_path_traversal_and_empty() {
        assert!(encode_session_id("").is_err());
        assert!(encode_session_id("   ").is_err());

        let traversal = encode_session_id("../../etc/passwd").expect("valid encoded");
        assert!(!traversal.contains('/'));
        assert!(!traversal.contains('\\'));
        assert!(!traversal.contains(".."));
        assert_eq!(traversal, "%2E%2E%2F%2E%2E%2Fetc%2Fpasswd");

        let slash = encode_session_id("a/b").expect("valid encoded");
        assert_eq!(slash, "a%2Fb");
        assert_ne!(
            encode_session_id("a/b").expect("a/b"),
            encode_session_id("a%2Fb").expect("a%2Fb")
        );

        let long_a = "x".repeat(250);
        let long_b = format!("{}y", "x".repeat(249));
        let enc_a = encode_session_id(&long_a).expect("long a");
        let enc_b = encode_session_id(&long_b).expect("long b");
        assert!(enc_a.len() < 200);
        assert_ne!(enc_a, enc_b);
    }

    #[test]
    fn test_prune_history_preserves_system_and_ensures_user_first() {
        let mut history = vec![
            msg("system", "sys"),
            msg("user", "u1"),
            msg("assistant", "a1"),
            msg("user", "u2"),
        ];

        prune_history(&mut history, 3);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], msg("system", "sys"));
        assert_eq!(history[1], msg("user", "u2"));
    }

    #[test]
    fn test_prune_history_without_system() {
        let mut history = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        prune_history(&mut history, 2);
        assert_eq!(history, vec![msg("user", "u2")]);
    }

    #[tokio::test]
    async fn test_save_and_reload_across_two_stores() {
        let guard = TempDirGuard::new("reload");
        let store1 = SessionStore::new(guard.path.clone(), 30);

        {
            let slot_arc = store1.acquire_slot("proj/review-1").await.expect("slot");
            let mut slot = slot_arc.lock().await;
            let session = store1
                .load_or_reset("proj/review-1", &mut slot, false)
                .await
                .expect("load");
            session.messages.push(msg("system", "be concise"));
            session.messages.push(msg("user", "hello"));
            session.messages.push(msg("assistant", "hi there"));
            store1
                .save_session(session, "gpt-6.1-sol")
                .await
                .expect("save");
        }

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&guard.path)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            let encoded = encode_session_id("proj/review-1").expect("encode");
            let file_mode = std::fs::metadata(guard.path.join(format!("{encoded}.json")))
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
        }

        // Second independent SessionStore lazily loads from disk
        let store2 = SessionStore::new(guard.path.clone(), 30);
        let loaded = store2
            .get_session("proj/review-1")
            .await
            .expect("get_session")
            .expect("session exists");
        assert_eq!(loaded.session_id, "proj/review-1");
        assert_eq!(loaded.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[0], msg("system", "be concise"));
        assert_eq!(loaded.messages[2], msg("assistant", "hi there"));
    }

    #[tokio::test]
    async fn test_reset_session_deletes_disk_file() {
        let guard = TempDirGuard::new("reset");
        let store = SessionStore::new(guard.path.clone(), 30);

        let slot_arc = store.acquire_slot("s1").await.expect("slot");
        {
            let mut slot = slot_arc.lock().await;
            let session = store
                .load_or_reset("s1", &mut slot, false)
                .await
                .expect("load");
            session.messages.push(msg("user", "hi"));
            session.messages.push(msg("assistant", "ok"));
            store.save_session(session, "m1").await.expect("save");
        }

        let file_path = guard.path.join("s1.json");
        assert!(file_path.exists());

        // Reset from a fresh store instance (unloaded in memory, present on disk)
        let store2 = SessionStore::new(guard.path.clone(), 30);
        let discarded = store2.reset_session("s1").await.expect("reset");
        assert_eq!(discarded, Some(2));
        assert!(!file_path.exists());
        assert_eq!(store2.get_session("s1").await.expect("get"), None);
        assert_eq!(store2.reset_session("s1").await.expect("reset again"), None);
    }

    #[tokio::test]
    async fn test_list_sessions_sorted_and_skips_corrupted() {
        let guard = TempDirGuard::new("list");
        let store = SessionStore::new(guard.path.clone(), 30);

        for (id, model, ts) in [("older", "m1", 100u64), ("newer", "m2", 200u64)] {
            let session = PersistedSession {
                session_id: id.to_string(),
                model: Some(model.to_string()),
                updated_at_unix: ts,
                messages: vec![msg("user", "q"), msg("assistant", "a")],
            };
            // Write directly via atomic writer so timestamps are deterministic
            store.ensure_sessions_dir().await.expect("dir");
            let path = store.session_file_path(id).expect("path");
            let json = serde_json::to_string_pretty(&session).expect("json");
            std::fs::write(path, json).expect("write");
        }

        // Write a corrupted .json file and a .tmp file that must both be ignored gracefully
        std::fs::write(guard.path.join("broken.json"), "{not valid json").expect("write broken");
        std::fs::write(guard.path.join("ignored.json.tmp.123"), "{}").expect("write tmp");

        let list = store.list_sessions().await.expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "newer");
        assert_eq!(list[0].updated_at_unix, 200);
        assert_eq!(list[1].session_id, "older");
        assert_eq!(list[1].updated_at_unix, 100);
    }

    #[tokio::test]
    async fn test_corrupted_session_on_get_returns_error() {
        let guard = TempDirGuard::new("corrupt-get");
        let store = SessionStore::new(guard.path.clone(), 30);
        store.ensure_sessions_dir().await.expect("dir");
        std::fs::write(guard.path.join("bad.json"), "corrupt!").expect("write");

        let res = store.get_session("bad").await;
        assert!(matches!(
            res,
            Err(SessionStoreError::CorruptedSession { .. })
        ));
    }

    #[tokio::test]
    async fn test_save_session_applies_max_history_messages() {
        let guard = TempDirGuard::new("prune-save");
        let store = SessionStore::new(guard.path.clone(), 3);

        let slot_arc = store.acquire_slot("trimmed").await.expect("slot");
        {
            let mut slot = slot_arc.lock().await;
            let session = store
                .load_or_reset("trimmed", &mut slot, false)
                .await
                .expect("load");
            session.messages = vec![
                msg("system", "sys"),
                msg("user", "u1"),
                msg("assistant", "a1"),
                msg("user", "u2"),
                msg("assistant", "a2"),
            ];
            store.save_session(session, "m1").await.expect("save");
        }

        let store2 = SessionStore::new(guard.path.clone(), 3);
        let reloaded = store2
            .get_session("trimmed")
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(
            reloaded.messages,
            vec![
                msg("system", "sys"),
                msg("user", "u2"),
                msg("assistant", "a2"),
            ]
        );
    }
}
