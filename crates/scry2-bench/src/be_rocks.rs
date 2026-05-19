//! RocksDB backend. Opt-in via `--features rocksdb-backend` because
//! the bindings drag a 5-min C++ build the first time around.
//!
//! Configured for read-heavy 13-byte fixed-prefix keys:
//!   * prefix extractor = 4 bytes (sym_id) so the prefix bloom filter
//!     can cheaply reject queries for absent symbols;
//!   * lz4 block compression — cheap CPU, big space win on packed keys;
//!   * 256 MB block cache;
//!   * `disable_wal` during bulk write (we fsync once at the end).

#![cfg(feature = "rocksdb-backend")]

use crate::backend::Backend;
use crate::workload::XKey;
use anyhow::{Context, Result};
use rocksdb::{
    BlockBasedOptions, Cache, DB, DBCompressionType, Options, ReadOptions,
    SliceTransform, WriteBatch, WriteOptions,
};
use std::path::PathBuf;

pub struct RocksBackend {
    path: PathBuf,
    db:   Option<DB>,
}

fn opts() -> Options {
    let mut o = Options::default();
    o.create_if_missing(true);
    o.set_compression_type(DBCompressionType::Lz4);
    o.set_prefix_extractor(SliceTransform::create_fixed_prefix(4));
    let mut bbt = BlockBasedOptions::default();
    let cache = Cache::new_lru_cache(256 * 1024 * 1024);
    bbt.set_block_cache(&cache);
    bbt.set_bloom_filter(10.0, false);
    bbt.set_whole_key_filtering(false);
    o.set_block_based_table_factory(&bbt);
    o
}

impl RocksBackend {
    pub fn create(path: PathBuf) -> Result<Self> {
        let _ = std::fs::remove_dir_all(&path);
        let db = DB::open(&opts(), &path).context("rocks: open")?;
        Ok(Self { path, db: Some(db) })
    }
}

impl Backend for RocksBackend {
    fn name(&self) -> &'static str { "rocksdb" }
    fn paths(&self) -> Vec<PathBuf> { vec![self.path.clone()] }

    fn bulk_write(&mut self, rows: &[XKey]) -> Result<()> {
        let db = self.db.as_ref().context("rocks: closed")?;
        let mut wo = WriteOptions::default();
        wo.disable_wal(true);
        const CHUNK: usize = 100_000;
        for slice in rows.chunks(CHUNK) {
            let mut b = WriteBatch::default();
            for k in slice {
                b.put(k.as_slice(), &[]);
            }
            db.write_opt(b, &wo).context("rocks: write_opt")?;
        }
        db.flush().context("rocks: flush")?;
        Ok(())
    }

    fn reopen_readonly(&mut self) -> Result<()> {
        self.db = None;
        let db = DB::open_for_read_only(&opts(), &self.path, /*error_if_log_file_exist=*/ false)
            .context("rocks: reopen ro")?;
        self.db = Some(db);
        Ok(())
    }

    fn prefix_count(&self, start: &XKey, end: &XKey) -> Result<usize> {
        let db = self.db.as_ref().context("rocks: closed")?;
        let mut ro = ReadOptions::default();
        ro.set_iterate_lower_bound(start.to_vec());
        ro.set_iterate_upper_bound(end.to_vec());
        ro.set_prefix_same_as_start(true);
        let mut it = db.raw_iterator_opt(ro);
        it.seek(start.as_slice());
        let mut n = 0usize;
        while it.valid() {
            n += 1;
            it.next();
        }
        Ok(n)
    }

    fn point_first(&self, start: &XKey, end: &XKey) -> Result<Option<XKey>> {
        let db = self.db.as_ref().context("rocks: closed")?;
        let mut ro = ReadOptions::default();
        ro.set_iterate_lower_bound(start.to_vec());
        ro.set_iterate_upper_bound(end.to_vec());
        let mut it = db.raw_iterator_opt(ro);
        it.seek(start.as_slice());
        if it.valid() {
            if let Some(k) = it.key() {
                if k.len() == 13 {
                    let mut out = [0u8; 13];
                    out.copy_from_slice(k);
                    return Ok(Some(out));
                }
            }
        }
        Ok(None)
    }
}
