//! My Domain's files (design domain-eng d1, 1.2; @coordinator-opus): the local home domain, kept in
//! `domains/home/` beside the identity file (the per-user data folder, app.md 4 step 2).
//!
//! - **`content.bin`**: the home's content (pe-world's `content::Content`), the player's own objects. The first run
//!   finds none and writes the starter set (pe-world's `preset::home::starter`), so a new home holds what ships with
//!   the game (C7: first run only; Restore starter set adds the rest later).
//! - **`domain.toml`**: the home's label and settings, a few `key = value` lines (strings quoted, `true`/`false`).
//!   Keys this build doesn't know are kept as they are, so an older build never drops a newer one's settings.
//! - **Saving is atomic and bounded** ("as long as it doesnt break the disk"): the new bytes go to a
//!   temporary file that is then renamed over `content.bin`, after the last [`KEEP`] saves move down one
//!   (`content.bin.1` is the newest older one). Nothing else is kept: the full history is the M3+ store's.
//! - **A damaged file never loses the home:** if `content.bin` doesn't decode, the newest older save that does is
//!   used, and the damaged file is kept beside it as `content.bin.damaged-<unix seconds>` for a person to look at.
//!   Only when nothing decodes does the home start over from the starter set.
//! - **Swappable:** @fable-reviewer's git layout (M3+) replaces these files; nothing outside this module knows them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pe_world::content::Content;

/// The home's folder, relative to the per-user data folder.
pub const DIR: [&str; 2] = ["domains", "home"];
/// The content file.
pub const CONTENT: &str = "content.bin";
/// The settings file.
pub const SETTINGS: &str = "domain.toml";
/// How many older saves are kept beside the current one.
pub const KEEP: usize = 5;
/// The home's label until a player renames it.
pub const DEFAULT_LABEL: &str = "My Domain";

/// The home's settings (`domain.toml`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub label: String,
    /// Whether others may browse the home without entering it (C10: off by default).
    pub browsable: bool,
    /// Lines this build doesn't read, kept in order.
    pub other: Vec<(String, String)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { label: DEFAULT_LABEL.into(), browsable: false, other: Vec::new() }
    }
}

impl Settings {
    /// The value of a key this build doesn't read itself (another module's setting), as written.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.other.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Sets another module's value (written as it is given: quote a string).
    pub fn set(&mut self, key: &str, value: &str) {
        match self.other.iter_mut().find(|(k, _)| k == key) {
            Some(kv) => kv.1 = value.into(),
            None => self.other.push((key.into(), value.into())),
        }
    }

    pub fn encode(&self) -> String {
        let mut s = String::from("# My Domain's settings (Primordial Engine). Edited by the game; safe to read.\n");
        s.push_str(&format!("format = 1\nlabel = {}\nbrowsable = {}\n", quote(&self.label), self.browsable));
        for (k, v) in &self.other {
            s.push_str(&format!("{k} = {v}\n"));
        }
        s
    }

    /// Reads [`Settings::encode`]'s lines; a line it can't read is skipped, a missing key keeps its default.
    pub fn decode(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "format" => {}
                "label" => {
                    if let Some(l) = unquote(v).filter(|l| !l.is_empty()) {
                        s.label = l;
                    }
                }
                "browsable" => s.browsable = v == "true",
                _ if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') => {
                    s.other.push((k.into(), v.into()));
                }
                _ => {}
            }
        }
        s
    }
}

fn quote(s: &str) -> String {
    let mut q = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            c if c.is_control() => {}
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

fn unquote(v: &str) -> Option<String> {
    let inner = v.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next()? {
                'n' => out.push('\n'),
                c => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// What [`Store::load`] found.
#[derive(Debug)]
pub struct Loaded {
    pub content: Content,
    pub settings: Settings,
    /// What happened, for the console and the log: "new home", "opened", or which older save was used and why.
    pub note: String,
}

/// The home's folder on disk.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The home beside the identity file `identity` (`<its folder>/domains/home`).
    pub fn beside(identity: &Path) -> Store {
        let base = identity.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        Store { dir: DIR.iter().fold(base, |p, d| p.join(d)) }
    }

    /// A home in `dir` itself (tests).
    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn content_path(&self) -> PathBuf {
        self.dir.join(CONTENT)
    }

    fn older(&self, k: usize) -> PathBuf {
        self.dir.join(format!("{CONTENT}.{k}"))
    }

    /// The home's content and settings. With no content file (the first run) it is the starter set, which is written
    /// at once so the home exists from now on.
    pub fn load(&self) -> io::Result<Loaded> {
        fs::create_dir_all(&self.dir)?;
        let settings = match fs::read_to_string(self.dir.join(SETTINGS)) {
            Ok(t) => Settings::decode(&t),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let s = Settings::default();
                self.save_settings(&s)?;
                s
            }
            Err(e) => return Err(e),
        };
        let path = self.content_path();
        let first = match fs::read(&path) {
            Ok(b) => Some(Content::decode(&b).map_err(|e| format!("{e:?}"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let (content, note) = match first {
            None => {
                let c = pe_world::preset::home::starter();
                self.save(&c)?;
                (c, "new home: the starter set".to_string())
            }
            Some(Ok(c)) => (c, "opened".to_string()),
            Some(Err(why)) => {
                let kept = self.dir.join(format!("{CONTENT}.damaged-{}", unix_s()));
                fs::rename(&path, &kept)?;
                let older = (1..=KEEP).find_map(|k| {
                    let b = fs::read(self.older(k)).ok()?;
                    Content::decode(&b).ok().map(|c| (k, c))
                });
                match older {
                    Some((k, c)) => {
                        self.write_current(&c)?;
                        (
                            c,
                            format!(
                                "{CONTENT} didn't open ({why}); opened the save before it ({CONTENT}.{k}); kept the damaged file as {}",
                                kept.display()
                            ),
                        )
                    }
                    None => {
                        let c = pe_world::preset::home::starter();
                        self.write_current(&c)?;
                        (
                            c,
                            format!(
                                "{CONTENT} didn't open ({why}) and no older save did; started over from the starter set; kept the damaged file as {}",
                                kept.display()
                            ),
                        )
                    }
                }
            }
        };
        Ok(Loaded { content, settings, note })
    }

    /// Saves `content` as the current home: the older saves move down one (the oldest beyond [`KEEP`] goes), then the
    /// new bytes replace `content.bin` atomically.
    pub fn save(&self, content: &Content) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let cur = self.content_path();
        if cur.exists() {
            for k in (1..KEEP).rev() {
                let from = self.older(k);
                if from.exists() {
                    fs::rename(&from, self.older(k + 1))?;
                }
            }
            fs::copy(&cur, self.older(1))?;
        }
        self.write_current(content)
    }

    fn write_current(&self, content: &Content) -> io::Result<()> {
        atomic_write(&self.content_path(), &content.encode())
    }

    pub fn save_settings(&self, s: &Settings) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        atomic_write(&self.dir.join(SETTINGS), s.encode().as_bytes())
    }
}

/// Writes `bytes` to a temporary file beside `path`, flushes it, and renames it over `path`.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn unix_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pe-home-{name}-{}-{}", std::process::id(), unix_s()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_first_run_writes_the_starter_set_and_default_settings() {
        let dir = temp("first");
        let store = Store::at(dir.clone());
        let l = store.load().unwrap();
        assert_eq!(l.content, pe_world::preset::home::starter());
        assert_eq!(l.settings, Settings::default());
        assert!(l.note.starts_with("new home"), "{}", l.note);
        assert!(dir.join(CONTENT).exists() && dir.join(SETTINGS).exists());
        let again = store.load().unwrap();
        assert_eq!((again.content, again.note.as_str()), (l.content, "opened"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_save_keeps_the_last_five_older_saves() {
        let dir = temp("keep");
        let store = Store::at(dir.clone());
        let starter = store.load().unwrap().content;
        for k in 0..8 {
            let mut c = starter.clone();
            c.items.truncate(k + 1);
            store.save(&c).unwrap();
        }
        for k in 1..=KEEP {
            assert!(dir.join(format!("{CONTENT}.{k}")).exists(), "{k}");
        }
        assert!(!dir.join(format!("{CONTENT}.{}", KEEP + 1)).exists());
        assert_eq!(store.load().unwrap().content.items.len(), 8);
        let one = Content::decode(&fs::read(dir.join(format!("{CONTENT}.1"))).unwrap()).unwrap();
        assert_eq!(one.items.len(), 7, "content.bin.1 is the newest older save");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_opens_the_save_before_it_and_is_kept() {
        let dir = temp("damaged");
        let store = Store::at(dir.clone());
        let starter = store.load().unwrap().content;
        let mut smaller = starter.clone();
        smaller.items.truncate(1);
        store.save(&smaller).unwrap();
        fs::write(dir.join(CONTENT), b"not content").unwrap();
        let l = store.load().unwrap();
        assert_eq!(l.content, starter, "the save before the damaged one");
        assert!(l.note.contains("content.bin.1"), "{}", l.note);
        let damaged = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("content.bin.damaged-"));
        assert!(damaged);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_round_trip_and_keep_unknown_keys() {
        let mut s = Settings { label: "Robin's \"place\"".into(), browsable: true, other: Vec::new() };
        s.set("visibility", "\"lan\"");
        let back = Settings::decode(&s.encode());
        assert_eq!(back, s);
        assert_eq!(back.get("visibility"), Some("\"lan\""));
        assert_eq!(Settings::decode("label = \nbrowsable = maybe\n= x\n"), Settings::default());
    }
}
