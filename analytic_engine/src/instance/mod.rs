// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

//! A table engine instance
//!
//! The root mod only contains common functions of instance, other logics are
//! divided into the sub crates

pub(crate) mod alter;
mod close;
mod create;
mod drop;
pub mod engine;
pub mod flush_compaction;
pub(crate) mod mem_collector;
pub mod open;
mod read;
pub(crate) mod write;
pub mod write_worker;

use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, RwLock},
};

use common_types::table::TableId;
use common_util::{define_result, runtime::Runtime};
use log::info;
use mem_collector::MemUsageCollector;
use snafu::{ResultExt, Snafu};
use table_engine::engine::EngineRuntimes;
use wal::manager::{WalLocation, WalManagerRef};

use crate::{
    compaction::scheduler::CompactionSchedulerRef,
    manifest::ManifestRef,
    row_iter::IterOptions,
    space::{SpaceId, SpaceRef},
    sst::{
        factory::{FactoryRef as SstFactoryRef, ObjectStorePickerRef, ScanOptions},
        file::FilePurger,
        meta_data::cache::MetaCacheRef,
    },
    table::data::{TableDataRef, TableShardInfo},
    TableOptions,
};

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to stop file purger, err:{}", source))]
    StopFilePurger { source: crate::sst::file::Error },

    #[snafu(display("Failed to stop compaction scheduler, err:{}", source))]
    StopScheduler {
        source: crate::compaction::scheduler::Error,
    },
}

define_result!(Error);

/// Spaces states
#[derive(Default)]
struct Spaces {
    /// Id to space
    id_to_space: HashMap<SpaceId, SpaceRef>,
}

impl Spaces {
    /// Insert space by name, and also insert id to space mapping
    fn insert(&mut self, space: SpaceRef) {
        let space_id = space.id;
        self.id_to_space.insert(space_id, space);
    }

    fn get_by_id(&self, id: SpaceId) -> Option<&SpaceRef> {
        self.id_to_space.get(&id)
    }

    /// List all tables of all spaces
    fn list_all_tables(&self, tables: &mut Vec<TableDataRef>) {
        let total_tables = self.id_to_space.values().map(|s| s.table_num()).sum();
        tables.reserve(total_tables);
        for space in self.id_to_space.values() {
            space.list_all_tables(tables);
        }
    }

    fn list_all_spaces(&self) -> Vec<SpaceRef> {
        self.id_to_space.values().cloned().collect()
    }
}

pub struct SpaceStore {
    /// All spaces of the engine.
    spaces: RwLock<Spaces>,
    /// Manifest (or meta) stores meta data of the engine instance.
    manifest: ManifestRef,
    /// Wal of all tables
    wal_manager: WalManagerRef,
    /// Object store picker for persisting data.
    store_picker: ObjectStorePickerRef,
    /// Sst factory.
    sst_factory: SstFactoryRef,

    meta_cache: Option<MetaCacheRef>,
}

impl Drop for SpaceStore {
    fn drop(&mut self) {
        info!("SpaceStore dropped");
    }
}

impl SpaceStore {
    async fn close(&self) -> Result<()> {
        let spaces = self.spaces.read().unwrap().list_all_spaces();
        for space in spaces {
            // Close all spaces.
            space.close().await;
        }

        Ok(())
    }
}

impl SpaceStore {
    fn store_picker(&self) -> &ObjectStorePickerRef {
        &self.store_picker
    }

    /// List all tables of all spaces
    pub fn list_all_tables(&self, tables: &mut Vec<TableDataRef>) {
        let spaces = self.spaces.read().unwrap();
        spaces.list_all_tables(tables);
    }

    /// Find the space which it's all memtables consumes maximum memory.
    #[inline]
    fn find_maximum_memory_usage_space(&self) -> Option<SpaceRef> {
        let spaces = self.spaces.read().unwrap().list_all_spaces();
        spaces.into_iter().max_by_key(|t| t.memtable_memory_usage())
    }
}

/// Table engine instance
///
/// Manages all spaces, also contains needed resources shared across all table
pub struct Instance {
    /// Space storage
    space_store: Arc<SpaceStore>,
    /// Runtime to execute async tasks.
    runtimes: Arc<EngineRuntimes>,
    /// Global table options, overwrite mutable options in each table's
    /// TableOptions.
    table_opts: TableOptions,

    // Write group options:
    write_group_worker_num: usize,
    write_group_command_channel_cap: usize,
    // End of write group options.
    compaction_scheduler: CompactionSchedulerRef,
    file_purger: FilePurger,

    meta_cache: Option<MetaCacheRef>,
    /// Engine memtable memory usage collector
    mem_usage_collector: Arc<MemUsageCollector>,
    /// Engine write buffer size
    pub(crate) db_write_buffer_size: usize,
    /// Space write buffer size
    pub(crate) space_write_buffer_size: usize,
    /// Replay wal batch size
    pub(crate) replay_batch_size: usize,
    /// Write sst max buffer size
    pub(crate) write_sst_max_buffer_size: usize,
    /// Max bytes per write batch
    pub(crate) max_bytes_per_write_batch: Option<usize>,
    /// Options for scanning sst
    pub(crate) scan_options: ScanOptions,
    pub(crate) iter_options: Option<IterOptions>,
    last_sequence: AtomicU64,
}

impl Instance {
    /// Close the instance gracefully.
    pub async fn close(&self) -> Result<()> {
        self.file_purger.stop().await.context(StopFilePurger)?;

        self.space_store.close().await?;

        self.compaction_scheduler
            .stop_scheduler()
            .await
            .context(StopScheduler)
    }
}

// TODO(yingwen): Instance builder
impl Instance {
    /// Find space using read lock
    fn get_space_by_read_lock(&self, space: SpaceId) -> Option<SpaceRef> {
        let spaces = self.space_store.spaces.read().unwrap();
        spaces.get_by_id(space).cloned()
    }

    /// Returns options to create a write group for given space
    fn write_group_options(&self, space_id: SpaceId) -> write_worker::Options {
        write_worker::Options {
            space_id,
            worker_num: self.write_group_worker_num,
            runtime: self.write_runtime().clone(),
            command_channel_capacity: self.write_group_command_channel_cap,
        }
    }

    /// Returns true when engine instance's total memtable memory usage reaches
    /// db_write_buffer_size limit.
    #[inline]
    fn should_flush_instance(&self) -> bool {
        self.db_write_buffer_size > 0
            && self.mem_usage_collector.total_memory_allocated() >= self.db_write_buffer_size
    }

    #[inline]
    fn read_runtime(&self) -> &Arc<Runtime> {
        &self.runtimes.read_runtime
    }

    #[inline]
    fn write_runtime(&self) -> &Arc<Runtime> {
        &self.runtimes.write_runtime
    }
}

/// Instance reference
pub type InstanceRef = Arc<Instance>;

#[inline]
pub(crate) fn create_wal_location(table_id: TableId, shard_info: TableShardInfo) -> WalLocation {
    WalLocation::new(shard_info.shard_id as u64, table_id)
}
