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
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

impl Store {
    pub fn set_dir(&self, dir: PathBuf) {
        let _ = self.dir.set(dir);
    }

    /// The newest valid record, or None (no file, or none readable: unreadable files are kept
    /// as they are and reported).
    pub fn load(&self) -> Option<Record> {
        let dir = self.dir.get()?;
        let mut best: Option<(usize, Record)> = None;
        for (i, name) in SLOTS.iter().enumerate() {
            let path = dir.join(name);
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    eprintln!("orchd: reading {}: {e}", path.display());
                    continue;
                }
            };
            match serde_json::from_slice::<Record>(&data) {
                Ok(r) if r.format == FORMAT => {
                    if best
                        .as_ref()
                        .is_none_or(|(_, b)| r.generation > b.generation)
                    {
                        best = Some((i, r));
                    }
                }
                Ok(r) => eprintln!(
                    "orchd: {} has format {} (this orchd reads {FORMAT}); left alone",
                    path.display(),
                    r.format
                ),
                Err(e) => eprintln!(
                    "orchd: {} is not readable ({e}); left alone",
                    path.display()
                ),
            }
        }
        let (i, r) = best?;
        let mut st = self.state.lock().unwrap();
        st.generation = r.generation;
        st.newest = i;
        Some(r)
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
    ) -> std::io::Result<()> {
        let Some(dir) = self.dir.get() else {
            return Err(std::io::Error::other("no folder for the sleeper record"));
        };
        if crate::fault::on("store_write") {
            return Err(std::io::Error::other("injected store write failure"));
        }
        let record = Record {
            format: FORMAT,
            generation: st.generation + 1,
            next_id,
            sleeping: sleeping.to_vec(),
        };
        let data = serde_json::to_vec_pretty(&record).map_err(std::io::Error::from)?;
        let slot = 1 - st.newest;
        let path = dir.join(SLOTS[slot]);
        crate::restore::write_atomic(&path, &data)?;
        fsync_dir(dir)?;
        st.generation = record.generation;
        st.newest = slot;
        Ok(())
    }
}
