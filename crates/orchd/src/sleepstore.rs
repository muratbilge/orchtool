//! The durable record of sleeping workspaces, kept apart from the layout files.
//!
//! Two slots next to the socket, `sleeping.a.json` and `sleeping.b.json`, each a complete
//! record `{format, generation, next_id, sleeping}`. A write goes to the slot not holding the
//! newest generation (tmp file, fsync, rename, fsync of the folder), so the previous record
//! stays intact until the new one is on disk; reading takes the valid slot with the highest
//! generation. Nothing deletes these files: not a plain `stop-daemon`, not a restart, not a
//! failed restore. `restore.json` / `autosave.json` (which an older orchd also reads and
//! rewrites) never contain sleeping workspaces, so a downgrade leaves this record alone: the
//! older orchd simply does not show them, and they are back after the next upgrade.
//!
//! Every write is of the daemon's current state, taken under the store's lock: writes are
//! serialized and a later write can never be overtaken by an older state.

use orch_proto::layout::Sleeping;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

pub const SLOTS: [&str; 2] = ["sleeping.a.json", "sleeping.b.json"];
const FORMAT: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub format: u32,
    pub generation: u64,
    /// The layout's id counter (workspace / tab ids of sleepers must never be handed out again).
    pub next_id: u64,
    pub sleeping: Vec<Sleeping>,
}

#[derive(Default)]
pub struct Store {
    dir: OnceLock<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
pub struct State {
    generation: u64,
    /// Slot index holding `generation`.
    newest: usize,
    /// Why the record must not be written (a slot that exists but can't be read, or is in a
    /// format this orchd does not know): writing could overwrite the only good copy.
    blocked: Option<String>,
}

/// A failed write. `published`: the new slot was renamed into place but its folder sync
/// failed: it may or may not survive a power loss. The caller treats it as failed (ends
/// nothing); a crash then makes the next start see a sleeper whose workspace is still in the
/// awake layout, and the awake copy wins.
#[derive(Debug)]
pub struct WriteError {
    pub published: bool,
    pub error: std::io::Error,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// What `load` found.
pub enum Loaded {
    /// No slot at all (a fresh installation).
    Missing,
    /// The newest readable record; `problem` names a slot that exists but was not usable.
    Found {
        record: Record,
        problem: Option<String>,
    },
    /// Slots exist, none usable.
    Unusable(String),
}

impl Store {
    pub fn set_dir(&self, dir: PathBuf) {
        let _ = self.dir.set(dir);
    }

    /// Both slots: the newest usable record (slot, record) and what was wrong with the others.
    fn scan(dir: &Path) -> (Option<(usize, Record)>, Vec<String>, bool) {
        let mut best: Option<(usize, Record)> = None;
        let mut problems = Vec::new();
        let mut any = false;
        for (i, name) in SLOTS.iter().enumerate() {
            let path = dir.join(name);
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    any = true;
                    problems.push(format!("{} can't be read: {e}", path.display()));
                    continue;
                }
            };
            any = true;
            match serde_json::from_slice::<Record>(&data) {
                Ok(r) if r.format == FORMAT => {
                    if best
                        .as_ref()
                        .is_none_or(|(_, b)| r.generation > b.generation)
                    {
                        best = Some((i, r));
                    }
                }
                Ok(r) => problems.push(format!(
                    "{} has format {} (this orchd reads {FORMAT})",
                    path.display(),
                    r.format
                )),
                Err(e) => problems.push(format!("{} is damaged ({e})", path.display())),
            }
        }
        (best, problems, any)
    }

    /// Reads both slots. A slot that exists but can't be read or has an unknown format is
    /// left alone **and blocks writes** (it may be the newest generation, or a newer orchd's).
    /// The block is checked again before every write: once the bad slot is moved away (or
    /// fixed), writes go on without a restart.
    pub fn load(&self) -> Loaded {
        let Some(dir) = self.dir.get() else {
            return Loaded::Missing;
        };
        let (best, problems, any) = Self::scan(dir);
        let problem = (!problems.is_empty()).then(|| problems.join("; "));
        let mut st = self.state.lock().unwrap();
        if let Some(p) = &problem {
            eprintln!("orchd: sleeping-workspace record: {p}; left alone, no writes until it is moved away");
            st.blocked = Some(p.clone());
        }
        match best {
            Some((i, record)) => {
                st.generation = record.generation;
                st.newest = i;
                Loaded::Found { record, problem }
            }
            None if any => Loaded::Unusable(problem.unwrap_or_default()),
            None => Loaded::Missing,
        }
    }

    /// A blocked store looks at its slots again. It unblocks only when no slot is bad any more
    /// **and** the newest record on disk is the one this orchd loaded (or there is none): a
    /// repaired slot that holds something else must be loaded by a restart, never overwritten
    /// with what is in memory.
    fn recheck(&self, st: &mut State) {
        let (Some(dir), Some(_)) = (self.dir.get(), &st.blocked) else {
            return;
        };
        let (best, problems, _) = Self::scan(dir);
        if !problems.is_empty() {
            st.blocked = Some(problems.join("; "));
            return;
        }
        match best {
            Some((i, r)) if r.generation == st.generation => {
                st.newest = i;
                st.blocked = None;
            }
            None if st.generation == 0 => st.blocked = None,
            Some((_, r)) => {
                st.blocked = Some(format!(
                    "the record on disk (generation {}) is not the one this orchd loaded (generation {}); restart orchd to load it",
                    r.generation, st.generation
                ))
            }
            None => {
                st.blocked = Some(
                    "the record this orchd loaded is gone from disk; restart orchd".into(),
                )
            }
        }
    }

    /// Whether a write would be attempted (checked before a wake starts anything).
    pub fn writable(&self) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        self.recheck(&mut st);
        match &st.blocked {
            Some(why) => Err(format!("the sleeper record can't be written: {why}")),
            None => Ok(()),
        }
    }

    /// The store's lock: hold it while reading the state to write, then `write`.
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Writes the next generation into the older slot. Without a folder there is nowhere to
    /// keep the record: an error, never a silent success.
    pub fn write(
        &self,
        st: &mut State,
        sleeping: &[Sleeping],
        next_id: u64,
    ) -> Result<(), WriteError> {
        let fail = |published, error| Err(WriteError { published, error });
        let Some(dir) = self.dir.get() else {
            return fail(
                false,
                std::io::Error::other("no folder for the sleeper record"),
            );
        };
        self.recheck(st);
        if let Some(why) = &st.blocked {
            return fail(false, std::io::Error::other(format!("not written: {why}")));
        }
        if let Some(ms) = crate::fault::value("store_slow_ms").and_then(|v| v.parse().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        if crate::fault::on("store_write") {
            return fail(false, std::io::Error::other("injected store write failure"));
        }
        let record = Record {
            format: FORMAT,
            generation: st.generation + 1,
            next_id,
            sleeping: sleeping.to_vec(),
        };
        let data =
            match orch_proto::layout::with_recovery_args(|| serde_json::to_vec_pretty(&record)) {
                Ok(d) => d,
                Err(e) => return fail(false, e.into()),
            };
        let slot = 1 - st.newest;
        let path = dir.join(SLOTS[slot]);
        let tmp = path.with_extension("json.new");
        let written = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            std::io::Write::write_all(&mut f, &data)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            return fail(false, e);
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            return fail(false, e);
        }
        // From here the new slot is visible: whatever happens, it is the newest generation.
        st.generation = record.generation;
        st.newest = slot;
        crate::fault::crash_at("store_crash_after_rename");
        let synced = if crate::fault::on("store_dir_sync") {
            Err(std::io::Error::other("injected folder sync failure"))
        } else {
            fsync_dir(dir)
        };
        if let Err(e) = synced {
            return fail(true, e);
        }
        Ok(())
    }
}
