//! redb backend. ACID B+tree, pure Rust, mmap'd reads.

use crate::backend::Backend;
use crate::workload::XKey;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::path::PathBuf;

const XREFS: TableDefinition<&[u8], ()> = TableDefinition::new("xrefs");

pub struct RedbBackend {
    path: PathBuf,
    db:   Option<Database>,
}

impl RedbBackend {
    pub fn create(path: PathBuf) -> Result<Self> {
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).context("redb: create")?;
        Ok(Self { path, db: Some(db) })
    }
}

impl Backend for RedbBackend {
    fn name(&self) -> &'static str { "redb" }
    fn paths(&self) -> Vec<PathBuf> { vec![self.path.clone()] }

    fn bulk_write(&mut self, rows: &[XKey]) -> Result<()> {
        let db = self.db.as_ref().context("redb: closed")?;
        let tx = db.begin_write()?;
        {
            let mut t = tx.open_table(XREFS)?;
            for k in rows {
                t.insert(k.as_slice(), ())?;
            }
        }
        tx.commit().context("redb: commit")?;
        Ok(())
    }

    fn reopen_readonly(&mut self) -> Result<()> {
        // Drop the writer handle, reopen for read.
        self.db = None;
        let db = Database::open(&self.path).context("redb: reopen")?;
        self.db = Some(db);
        Ok(())
    }

    fn prefix_count(&self, start: &XKey, end: &XKey) -> Result<usize> {
        let db = self.db.as_ref().context("redb: closed")?;
        let r = db.begin_read()?;
        let t = r.open_table(XREFS)?;
        Ok(t.range(start.as_slice()..end.as_slice())?.count())
    }

    fn point_first(&self, start: &XKey, end: &XKey) -> Result<Option<XKey>> {
        let db = self.db.as_ref().context("redb: closed")?;
        let r = db.begin_read()?;
        let t = r.open_table(XREFS)?;
        let mut it = t.range(start.as_slice()..end.as_slice())?;
        match it.next() {
            Some(Ok((k, _))) => {
                let bytes = k.value();
                if bytes.len() == 13 {
                    let mut out = [0u8; 13];
                    out.copy_from_slice(bytes);
                    Ok(Some(out))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }
}
