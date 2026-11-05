//! Persistence, in plain files. `document.json` holds the projects and settings and each conversation's items
//! are an append-only JSONL file, so opening the app never reads a conversation nobody asked for. Writes go
//! through one background thread: the UI thread never waits for a disk.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::model::{Document, Item};

enum Job {
    Document(Box<Document>),
    Append { conv: String, items: Vec<Item> },
    Rewrite { conv: String, items: Vec<Item> },
    Delete { conv: String },
    Flush(Sender<()>),
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    jobs: Sender<Job>,
}

pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MEWRK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // Deliberately not Mewrk's own folder: the two apps must be able to run side by side without touching each other's data.
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Application Support/dev.mewrk.native")
}

/// Temp file, flush, rename: a crash leaves the old file or the new one, never half of either.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path)
}

fn conv_path(dir: &Path, conv: &str) -> PathBuf {
    // Subagents keep their items under their parent, as `<parent>/<child>`.
    dir.join("conversations").join(format!("{conv}.jsonl"))
}

fn write_items(path: &Path, items: &[Item], append: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = if append { OpenOptions::new().create(true).append(true).open(path)? } else { File::create(path)? };
    let mut out = BufWriter::new(file);
    for item in items {
        serde_json::to_writer(&mut out, item).map_err(io::Error::other)?;
        out.write_all(b"\n")?;
    }
    out.flush()
}

impl Store {
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(dir.join("conversations"))?;
        let (jobs, rx) = mpsc::channel::<Job>();
        let worker_dir = dir.clone();
        thread::Builder::new().name("store".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                let result = match job {
                    Job::Document(mut doc) => {
                        // Only the newest document matters, so a burst of saves costs one write.
                        let mut flush = None;
                        let mut rest = Vec::new();
                        while let Ok(next) = rx.try_recv() {
                            match next {
                                Job::Document(newer) => doc = newer,
                                Job::Flush(tx) => flush = Some(tx),
                                other => rest.push(other),
                            }
                        }
                        let bytes = serde_json::to_vec_pretty(&*doc).map_err(io::Error::other);
                        let r = bytes.and_then(|b| atomic_write(&worker_dir.join("document.json"), &b));
                        for job in rest {
                            if let Err(e) = run_conv_job(&worker_dir, job) {
                                eprintln!("store: {e}");
                            }
                        }
                        if let Some(tx) = flush {
                            let _ = tx.send(());
                        }
                        r
                    }
                    Job::Flush(tx) => {
                        let _ = tx.send(());
                        Ok(())
                    }
                    other => run_conv_job(&worker_dir, other),
                };
                if let Err(e) = result {
                    eprintln!("store: {e}");
                }
            }
        })?;
        Ok(Self { dir, jobs })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_document(&self) -> Document {
        match fs::read(self.dir.join("document.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                // Never overwrite something that could not be read: keep it where a person can get at it.
                let keep = self.dir.join(format!("document.unreadable-{}.json", crate::model::now_ms()));
                let _ = fs::write(&keep, &bytes);
                eprintln!("document.json is unreadable ({e}), kept as {}", keep.display());
                Document::default()
            }),
            Err(_) => Document::default(),
        }
    }

    pub fn save_document(&self, doc: &Document) {
        let _ = self.jobs.send(Job::Document(Box::new(doc.clone())));
    }

    pub fn append(&self, conv: &str, items: Vec<Item>) {
        let _ = self.jobs.send(Job::Append { conv: conv.to_owned(), items });
    }

    pub fn rewrite(&self, conv: &str, items: Vec<Item>) {
        let _ = self.jobs.send(Job::Rewrite { conv: conv.to_owned(), items });
    }

    pub fn delete_conversation(&self, conv: &str) {
        let _ = self.jobs.send(Job::Delete { conv: conv.to_owned() });
    }

    /// Blocks until everything sent so far is on disk.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.jobs.send(Job::Flush(tx)).is_ok() {
            let _ = rx.recv();
        }
    }

    /// Reads a conversation. A line that does not parse (a crash mid-write leaves one at the end) is skipped.
    pub fn load_items(&self, conv: &str) -> io::Result<Vec<Item>> {
        load_items_from(&conv_path(&self.dir, conv))
    }
}

fn run_conv_job(dir: &Path, job: Job) -> io::Result<()> {
    match job {
        Job::Append { conv, items } => write_items(&conv_path(dir, &conv), &items, true),
        Job::Rewrite { conv, items } => {
            let path = conv_path(dir, &conv);
            let tmp = path.with_extension("jsonl.tmp");
            write_items(&tmp, &items, false)?;
            fs::rename(tmp, path)
        }
        Job::Delete { conv } => {
            let path = conv_path(dir, &conv);
            let children = path.with_extension("");
            if children.is_dir() {
                let _ = fs::remove_dir_all(children);
            }
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
        Job::Document(_) | Job::Flush(_) => Ok(()),
    }
}

pub fn load_items_from(path: &Path) -> io::Result<Vec<Item>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut items = Vec::new();
    let mut seen: HashMap<u64, usize> = HashMap::new();
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<Item>(&line) else { continue };
        // A later line for the same id is a newer state of that item.
        match seen.get(&item.id()) {
            Some(&i) => items[i] = item,
            None => {
                seen.insert(item.id(), items.len());
                items.push(item);
            }
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mewrk-store-{}", new_id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn items_survive_a_round_trip_and_later_lines_win() {
        let dir = tmp();
        let store = Store::open(dir.clone()).unwrap();
        store.append("c", vec![Item::Text { id: 1, text: "old".into(), tokens: 1 }, Item::User { id: 2, text: "q".into(), images: vec![], at: 5 }]);
        store.append("c", vec![Item::Text { id: 1, text: "new".into(), tokens: 1 }]);
        store.flush();
        let items = store.load_items("c").unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], Item::Text { text, .. } if text == "new"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_torn_last_line_loses_only_itself() {
        let dir = tmp();
        let store = Store::open(dir.clone()).unwrap();
        store.append("c", vec![Item::Text { id: 1, text: "kept".into(), tokens: 1 }]);
        store.flush();
        let path = conv_path(&dir, "c");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"t":"text","id":2,"te"#).unwrap();
        drop(f);
        assert_eq!(store.load_items("c").unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_document_round_trips_and_an_unreadable_one_is_kept() {
        let dir = tmp();
        let store = Store::open(dir.clone()).unwrap();
        let mut doc = Document::default();
        doc.projects.push(Project { id: "p".into(), name: "n".into(), path: Some("/x".into()), collapsed: false });
        store.save_document(&doc);
        store.flush();
        assert_eq!(store.load_document(), doc);
        fs::write(dir.join("document.json"), b"{nope").unwrap();
        assert_eq!(store.load_document(), Document::default());
        assert!(fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("document.unreadable")));
        fs::remove_dir_all(dir).unwrap();
    }
}
