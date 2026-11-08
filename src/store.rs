//! Local chat history and settings, stored as JSON files under the data dir:
//! `chats/<id>.json`, `attachments/<sha256>`, `settings.json`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const NEW_CHAT: &str = "New chat";
const TITLE_MAX_CHARS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Metadata only; the bytes live in `attachments/<sha256>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// Timestamps are unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub model: Option<String>,
    pub web_search: bool,
    pub instructions: String,
    pub welcome_seen: bool,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: PathBuf) -> io::Result<Store> {
        fs::create_dir_all(dir.join("chats"))?;
        fs::create_dir_all(dir.join("attachments"))?;
        Ok(Store { dir })
    }

    /// Not saved until `save` is called. Ids are UUIDv7, so they sort by creation time.
    pub fn new_chat(&self) -> Chat {
        let now = now();
        Chat {
            id: Uuid::now_v7().to_string(),
            title: NEW_CHAT.into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
        }
    }

    /// Newest `updated_at` first. Unreadable or corrupt files are skipped.
    pub fn list(&self) -> Vec<ChatSummary> {
        let Ok(entries) = fs::read_dir(self.dir.join("chats")) else {
            return Vec::new();
        };
        let mut chats: Vec<ChatSummary> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| serde_json::from_slice(&fs::read(p).ok()?).ok())
            .collect();
        chats.sort_by(|a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
        chats
    }

    pub fn load(&self, id: &str) -> io::Result<Chat> {
        Ok(serde_json::from_slice(&fs::read(self.chat_path(id)?)?)?)
    }

    pub fn save(&self, chat: &Chat) -> io::Result<()> {
        write_atomic(&self.chat_path(&chat.id)?, &serde_json::to_vec(chat)?)
    }

    // ponytail: attachment files are kept on delete (they may be shared by other chats); add GC if disk use matters.
    pub fn delete(&self, id: &str) -> io::Result<()> {
        fs::remove_file(self.chat_path(id)?)
    }

    /// Stores bytes content-addressed, so identical files are kept once.
    pub fn put_attachment(&self, name: &str, mime: &str, bytes: &[u8]) -> io::Result<Attachment> {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path = self.dir.join("attachments").join(&sha256);
        if !path.exists() {
            write_atomic(&path, bytes)?;
        }
        Ok(Attachment {
            name: name.into(),
            mime: mime.into(),
            sha256,
        })
    }

    pub fn attachment_bytes(&self, a: &Attachment) -> io::Result<Vec<u8>> {
        let valid = a.sha256.len() == 64
            && a.sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err(invalid("invalid attachment hash"));
        }
        fs::read(self.dir.join("attachments").join(&a.sha256))
    }

    /// Missing or corrupt settings yield the defaults.
    pub fn settings(&self) -> Settings {
        fs::read(self.settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) -> io::Result<()> {
        write_atomic(&self.settings_path(), &serde_json::to_vec_pretty(s)?)
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    /// Ids come from file contents and UI state, so only `[A-Za-z0-9_-]` is allowed (no path traversal).
    fn chat_path(&self, id: &str) -> io::Result<PathBuf> {
        let valid = !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(invalid("invalid chat id"));
        }
        Ok(self.dir.join("chats").join(format!("{id}.json")))
    }
}

/// First line, trimmed, at most 48 chars cut at a word boundary; "New chat" if empty.
pub fn title_from(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return NEW_CHAT.into();
    }
    let Some((cut, _)) = line.char_indices().nth(TITLE_MAX_CHARS) else {
        return line.into();
    };
    let head = &line[..cut];
    let head = if line[cut..].starts_with(char::is_whitespace) {
        head
    } else {
        // Back up to the last space; hard cut if there is none.
        head.rfind(char::is_whitespace).map_or(head, |i| &head[..i])
    };
    head.trim_end().into()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Writes to a unique temp file in the same dir, fsyncs, then renames over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", Uuid::now_v7().simple()));
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("patina-store-test-{}", Uuid::now_v7()));
        Store::open(dir).unwrap()
    }

    fn cleanup(s: Store) {
        let _ = fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn save_load_roundtrip() {
        let s = temp_store();
        let mut chat = s.new_chat();
        let a = s.put_attachment("cat.png", "image/png", b"png").unwrap();
        chat.title = title_from("Hello there");
        chat.messages = vec![
            Message {
                role: Role::User,
                text: "Hello there".into(),
                attachments: vec![a],
            },
            Message {
                role: Role::Assistant,
                text: "Hi!".into(),
                attachments: vec![],
            },
        ];
        s.save(&chat).unwrap();
        assert_eq!(s.load(&chat.id).unwrap(), chat);

        let json = fs::read_to_string(s.chat_path(&chat.id).unwrap()).unwrap();
        assert!(json.contains(r#""role":"user""#) && json.contains(r#""role":"assistant""#));
        // No temp files left behind.
        assert_eq!(fs::read_dir(s.dir.join("chats")).unwrap().count(), 1);
        cleanup(s);
    }

    #[test]
    fn new_chat_ids_are_unique_and_sortable() {
        let s = temp_store();
        let (a, b) = (s.new_chat(), s.new_chat());
        assert!(a.id < b.id);
        assert_eq!(a.title, "New chat");
        assert!(s.chat_path(&a.id).is_ok());
        cleanup(s);
    }

    #[test]
    fn list_newest_first_and_skips_bad_files() {
        let s = temp_store();
        for (id, t) in [("a", 10), ("b", 30), ("c", 20)] {
            let mut chat = s.new_chat();
            chat.id = id.into();
            chat.updated_at = t;
            s.save(&chat).unwrap();
        }
        fs::write(s.dir.join("chats/broken.json"), b"{not json").unwrap();
        // A leftover temp file holds a valid chat; it must not show up as a duplicate.
        let mut tmp = s.load("a").unwrap();
        tmp.updated_at = 99;
        fs::write(
            s.dir.join("chats/a.0123.tmp"),
            serde_json::to_vec(&tmp).unwrap(),
        )
        .unwrap();
        let ids: Vec<_> = s.list().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        cleanup(s);
    }

    #[test]
    fn delete_removes_chat() {
        let s = temp_store();
        let chat = s.new_chat();
        s.save(&chat).unwrap();
        s.delete(&chat.id).unwrap();
        assert_eq!(
            s.load(&chat.id).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(s.list().is_empty());
        cleanup(s);
    }

    #[test]
    fn rejects_bad_ids() {
        let s = temp_store();
        for id in ["../x", "", "a/b", "a.json", "..", &"x".repeat(65)] {
            let mut chat = s.new_chat();
            chat.id = id.into();
            assert_eq!(
                s.save(&chat).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{id}"
            );
            assert_eq!(s.load(id).unwrap_err().kind(), io::ErrorKind::InvalidInput);
            assert_eq!(
                s.delete(id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let bad = Attachment {
            name: "x".into(),
            mime: "text/plain".into(),
            sha256: "../../settings.json".into(),
        };
        assert_eq!(
            s.attachment_bytes(&bad).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        cleanup(s);
    }

    #[test]
    fn attachments_dedupe_by_hash() {
        let s = temp_store();
        let a = s.put_attachment("a.txt", "text/plain", b"abc").unwrap();
        let b = s.put_attachment("b.txt", "text/plain", b"abc").unwrap();
        assert_eq!(
            a.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(a.sha256, b.sha256);
        assert_eq!((a.name.as_str(), b.name.as_str()), ("a.txt", "b.txt"));
        assert_eq!(fs::read_dir(s.dir.join("attachments")).unwrap().count(), 1);
        assert_eq!(s.attachment_bytes(&b).unwrap(), b"abc");
        cleanup(s);
    }

    #[test]
    fn settings_default_on_missing_or_corrupt() {
        let s = temp_store();
        assert_eq!(s.settings(), Settings::default());

        fs::write(s.settings_path(), b"not json").unwrap();
        assert_eq!(s.settings(), Settings::default());

        fs::write(s.settings_path(), br#"{"web_search":true}"#).unwrap();
        assert!(s.settings().web_search);
        assert_eq!(s.settings().model, None);

        let set = Settings {
            model: Some("gpt-5".into()),
            web_search: true,
            instructions: "Be brief.".into(),
            welcome_seen: true,
        };
        s.save_settings(&set).unwrap();
        assert_eq!(s.settings(), set);
        cleanup(s);
    }

    #[test]
    fn title_from_cases() {
        assert_eq!(title_from(""), "New chat");
        assert_eq!(title_from("  \n\t \n"), "New chat");
        assert_eq!(title_from("  Hello world  \nsecond line"), "Hello world");
        assert_eq!(title_from("\n\nAfter blank lines"), "After blank lines");

        let exact = "a".repeat(48);
        assert_eq!(title_from(&exact), exact);

        // 49+ chars: cut back to the last word boundary.
        let long = "The quick brown fox jumps over the lazy dog again and again";
        assert_eq!(
            title_from(long),
            "The quick brown fox jumps over the lazy dog"
        );
        // Boundary falls exactly after a full word.
        let words = format!("{} tail", "b".repeat(48));
        assert_eq!(title_from(&words), "b".repeat(48));
        // One long word: hard cut at 48 chars.
        assert_eq!(title_from(&"x".repeat(60)), "x".repeat(48));
        // Counts chars, not bytes.
        let cyr = "Привет ".repeat(10);
        let t = title_from(&cyr);
        assert!(t.chars().count() <= 48 && t.ends_with("Привет"), "{t}");
    }
}
