// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

//! Open logic of instance

use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};

use common_types::schema::IndexInWriterSchema;
use log::{debug, error, info, trace, warn};
use snafu::ResultExt;
use table_engine::engine::OpenTableRequest;
use tokio::sync::oneshot;
use wal::{
    log_batch::LogEntry,
    manager::{ReadBoundary, ReadContext, ReadRequest, WalManagerRef},
};

use crate::{
    compaction::scheduler::SchedulerImpl,
    context::OpenContext,
    engine,
    instance::{
        self,
        engine::{
            ApplyMemTable, FlushTable, OperateByWriteWorker, ReadMetaUpdate, ReadWal,
            RecoverTableData, Result,
        },
        flush_compaction::TableFlushOptions,
        mem_collector::MemUsageCollector,
        write_worker,
        write_worker::{RecoverTableCommand, WorkerLocal, WriteGroup},
        Instance, SpaceStore, Spaces,
    },
    manifest::{meta_data::TableManifestData, LoadRequest, ManifestRef},
    payload::{ReadPayload, WalDecoder},
    row_iter::IterOptions,
    space::{Space, SpaceContext, SpaceId, SpaceRef},
    sst::{
        factory::{FactoryRef as SstFactoryRef, ObjectStorePickerRef, ScanOptions},
        file::FilePurger,
    },
    table::data::{TableData, TableDataRef},
};

const MAX_RECORD_BATCHES_IN_FLIGHT_WHEN_COMPACTION_READ: usize = 64;

impl Instance {
    /// Open a new instance
    pub async fn open(
        ctx: OpenContext,
        manifest: ManifestRef,
        wal_manager: WalManagerRef,
        store_picker: ObjectStorePickerRef,
        sst_factory: SstFactoryRef,
    ) -> Result<Arc<Self>> {
        let space_store = Arc::new(SpaceStore {
            spaces: RwLock::new(Spaces::default()),
            manifest,
            wal_manager: wal_manager.clone(),
            store_picker: store_picker.clone(),
            sst_factory,
            meta_cache: ctx.meta_cache.clone(),
        });

        let scheduler_config = ctx.config.compaction_config.clone();
        let scan_options_for_compaction = ScanOptions {
            background_read_parallelism: 1,
            max_record_batches_in_flight: MAX_RECORD_BATCHES_IN_FLIGHT_WHEN_COMPACTION_READ,
        };
        let compaction_runtime = ctx.runtimes.compact_runtime.clone();
        let compaction_scheduler = Arc::new(SchedulerImpl::new(
            space_store.clone(),
            compaction_runtime,
            scheduler_config,
            ctx.config.write_sst_max_buffer_size.as_byte() as usize,
            scan_options_for_compaction,
        ));

        let default_runtime = ctx.runtimes.default_runtime.clone();
        let file_purger = FilePurger::start(&default_runtime, store_picker.default_store().clone());

        let scan_options = ScanOptions {
            background_read_parallelism: ctx.config.sst_background_read_parallelism,
            max_record_batches_in_flight: ctx.config.scan_max_record_batches_in_flight,
        };

        let iter_options = ctx
            .config
            .scan_batch_size
            .map(|batch_size| IterOptions { batch_size });
        let instance = Arc::new(Instance {
            space_store,
            runtimes: ctx.runtimes.clone(),
            table_opts: ctx.config.table_opts.clone(),

            write_group_worker_num: ctx.config.write_group_worker_num,
            write_group_command_channel_cap: ctx.config.write_group_command_channel_cap,
            compaction_scheduler,
            file_purger,
            meta_cache: ctx.meta_cache.clone(),
            mem_usage_collector: Arc::new(MemUsageCollector::default()),
            db_write_buffer_size: ctx.config.db_write_buffer_size,
            space_write_buffer_size: ctx.config.space_write_buffer_size,
            replay_batch_size: ctx.config.replay_batch_size,
            write_sst_max_buffer_size: ctx.config.write_sst_max_buffer_size.as_byte() as usize,
            max_bytes_per_write_batch: ctx
                .config
                .max_bytes_per_write_batch
                .map(|v| v.as_byte() as usize),
            iter_options,
            scan_options,
            last_sequence: Default::default(),
        });

        Ok(instance)
    }

    /// Open the space if it is not opened before.
    async fn open_space(
        self: &Arc<Self>,
        space_id: SpaceId,
        context: SpaceContext,
    ) -> Result<SpaceRef> {
        {
            let spaces = self.space_store.spaces.read().unwrap();

            if let Some(space) = spaces.get_by_id(space_id) {
                return Ok(space.clone());
            }
        }

        // double check whether the space exists.
        let mut spaces = self.space_store.spaces.write().unwrap();
        if let Some(space) = spaces.get_by_id(space_id) {
            return Ok(space.clone());
        }

        // space is not opened yet and try to open it
        let write_group_opts = self.write_group_options(space_id);
        let write_group = WriteGroup::new(write_group_opts, self.clone());

        // Add this space to instance.
        let space = Arc::new(Space::new(
            space_id,
            context,
            self.space_write_buffer_size,
            write_group,
            self.mem_usage_collector.clone(),
        ));
        spaces.insert(space.clone());

        Ok(space)
    }

    /// Open the table.
    pub async fn do_open_table(
        self: &Arc<Self>,
        space: SpaceRef,
        request: &OpenTableRequest,
    ) -> Result<Option<TableDataRef>> {
        if let Some(table_data) = space.find_table_by_id(request.table_id) {
            return Ok(Some(table_data));
        }
        let table_data = match self.recover_table_meta_data(request).await? {
            Some(v) => v,
            None => return Ok(None),
        };

        let (tx, rx) = oneshot::channel();
        let cmd = RecoverTableCommand {
            space,
            table_data: table_data.clone(),
            tx,
            replay_batch_size: self.replay_batch_size,
        };

        // Send recover request to write worker, actual works done in
        // Self::recover_table_from_wal()
        write_worker::process_command_in_write_worker(cmd.into_command(), &table_data, rx)
            .await
            .context(OperateByWriteWorker {
                space_id: table_data.space_id,
                table: &table_data.name,
                table_id: table_data.id,
            })
    }

    /// Recover the table data.
    ///
    /// Return None if the table data does not exist.
    pub async fn process_recover_table_command(
        self: &Arc<Self>,
        worker_local: &mut WorkerLocal,
        space: SpaceRef,
        table_data: TableDataRef,
        replay_batch_size: usize,
    ) -> Result<Option<TableDataRef>> {
        if let Some(exist_table_data) = space.find_table_by_id(table_data.id) {
            warn!("Open a opened table, table:{}", table_data.name);
            return Ok(Some(exist_table_data));
        }

        let read_ctx = ReadContext {
            batch_size: replay_batch_size,
            ..Default::default()
        };

        self.recover_table_from_wal(
            worker_local,
            table_data.clone(),
            replay_batch_size,
            &read_ctx,
        )
        .await
        .map_err(|e| {
            error!("Recovery table from wal failed, table_data:{table_data:?}, err:{e}");
            space.insert_open_failed_table(table_data.name.to_string());
            e
        })?;

        space.insert_table(table_data.clone());
        Ok(Some(table_data))
    }

    /// Recover meta data from manifest
    ///
    /// Return None if no meta data is found for the table.
    async fn recover_table_meta_data(
        self: &Arc<Self>,
        request: &OpenTableRequest,
    ) -> Result<Option<TableDataRef>> {
        info!("Instance recover table:{} meta begin", request.table_id);

        // Load manifest, also create a new snapshot at startup.
        let table_id = request.table_id;
        let space_id = engine::build_space_id(request.schema_id);
        let load_req = LoadRequest {
            space_id,
            table_id,
            shard_id: request.shard_id,
        };
        let manifest_data = self
            .space_store
            .manifest
            .load_data(&load_req)
            .await
            .context(ReadMetaUpdate {
                table_id: request.table_id,
            })?;

        let table_data = if let Some(manifest_data) = manifest_data {
            Some(self.recover_table_data(manifest_data, request).await?)
        } else {
            None
        };

        info!("Instance recover table:{} meta end", request.table_id);

        Ok(table_data)
    }

    /// Recover `TableData` by applying manifest data to instance
    async fn recover_table_data(
        self: &Arc<Self>,
        manifest_data: TableManifestData,
        request: &OpenTableRequest,
    ) -> Result<TableDataRef> {
        let TableManifestData {
            table_meta,
            version_meta,
        } = manifest_data;

        let context = SpaceContext {
            catalog_name: request.catalog_name.clone(),
            schema_name: request.schema_name.clone(),
        };
        let space = self.open_space(table_meta.space_id, context).await?;

        let (table_id, table_name) = (table_meta.table_id, table_meta.table_name.clone());
        // Choose write worker for this table
        let write_handle = space.write_group.choose_worker(table_id);

        debug!("Instance apply add table, meta :{:?}", table_meta);

        let table_data = Arc::new(
            TableData::recover_from_add(
                table_meta.clone(),
                write_handle,
                &self.file_purger,
                space.mem_usage_collector.clone(),
                request.shard_id,
            )
            .context(RecoverTableData {
                space_id: table_meta.space_id,
                table: &table_name,
            })?,
        );
        // Apply version meta to the table.
        if let Some(version_meta) = version_meta {
            let max_file_id = version_meta.max_file_id_to_add();
            table_data.current_version().apply_meta(version_meta);
            // In recovery case, we need to maintain last file id of the table manually.
            if table_data.last_file_id() < max_file_id {
                table_data.set_last_file_id(max_file_id);
            }
        }

        Ok(table_data)
    }

    /// Recover table data from wal
    ///
    /// Called by write worker
    pub(crate) async fn recover_table_from_wal(
        self: &Arc<Self>,
        worker_local: &mut WorkerLocal,
        table_data: TableDataRef,
        replay_batch_size: usize,
        read_ctx: &ReadContext,
    ) -> Result<()> {
        debug!(
            "Instance recover table from wal, replay batch size:{}, table id:{}, shard info:{:?}",
            replay_batch_size, table_data.id, table_data.shard_info
        );

        let table_location = table_data.table_location();
        let wal_location =
            instance::create_wal_location(table_location.id, table_location.shard_info);
        let read_req = ReadRequest {
            location: wal_location,
            start: ReadBoundary::Excluded(table_data.current_version().flushed_sequence()),
            end: ReadBoundary::Max,
        };

        // Read all wal of current table.
        let mut log_iter = self
            .space_store
            .wal_manager
            .read_batch(read_ctx, &read_req)
            .await
            .context(ReadWal)?;

        let mut log_entry_buf = VecDeque::with_capacity(replay_batch_size);
        loop {
            // fetch entries to log_entry_buf
            let decoder = WalDecoder::default();
            log_entry_buf = log_iter
                .next_log_entries(decoder, log_entry_buf)
                .await
                .context(ReadWal)?;

            // Replay all log entries of current table
            self.replay_table_log_entries(worker_local, &table_data, &log_entry_buf)
                .await?;

            // No more entries.
            if log_entry_buf.is_empty() {
                break;
            }
        }

        Ok(())
    }

    /// Replay all log entries into memtable and flush if necessary.
    async fn replay_table_log_entries(
        self: &Arc<Self>,
        worker_local: &mut WorkerLocal,
        table_data: &TableDataRef,
        log_entries: &VecDeque<LogEntry<ReadPayload>>,
    ) -> Result<()> {
        if log_entries.is_empty() {
            info!(
                "Instance replay an empty table log entries, table:{}, table_id:{:?}",
                table_data.name, table_data.id
            );

            // No data in wal
            return Ok(());
        }

        let last_sequence = log_entries.back().unwrap().sequence;

        info!(
            "Instance replay table log entries begin, table:{}, table_id:{:?}, sequence:{}",
            table_data.name, table_data.id, last_sequence
        );

        for log_entry in log_entries {
            let (sequence, payload) = (log_entry.sequence, &log_entry.payload);

            // Apply to memtable
            match payload {
                ReadPayload::Write { row_group } => {
                    trace!(
                        "Instance replay row_group, table:{}, row_group:{:?}",
                        table_data.name,
                        row_group
                    );

                    let table_schema_version = table_data.schema_version();
                    if table_schema_version != row_group.schema().version() {
                        // Data with old schema should already been flushed, but we avoid panic
                        // here.
                        error!(
                            "Ignore data with mismatch schema version during replaying, \
                            table:{}, \
                            table_id:{:?}, \
                            expect:{}, \
                            actual:{}, \
                            last_sequence:{}, \
                            sequence:{}",
                            table_data.name,
                            table_data.id,
                            table_schema_version,
                            row_group.schema().version(),
                            last_sequence,
                            sequence,
                        );

                        continue;
                    }

                    let index_in_writer =
                        IndexInWriterSchema::for_same_schema(row_group.schema().num_columns());
                    Self::write_to_memtable(
                        worker_local,
                        table_data,
                        sequence,
                        &row_group.into(),
                        index_in_writer,
                    )
                    .context(ApplyMemTable {
                        space_id: table_data.space_id,
                        table: &table_data.name,
                        table_id: table_data.id,
                    })?;

                    // Flush the table if necessary.
                    if table_data.should_flush_table(worker_local) {
                        let opts = TableFlushOptions {
                            res_sender: None,
                            compact_after_flush: false,
                            block_on_write_thread: false,
                        };
                        self.flush_table_in_worker(worker_local, table_data, opts)
                            .await
                            .context(FlushTable {
                                space_id: table_data.space_id,
                                table: &table_data.name,
                                table_id: table_data.id,
                            })?;
                    }
                }
                ReadPayload::AlterSchema { .. } | ReadPayload::AlterOptions { .. } => {
                    // Ignore records except Data.
                    //
                    // - DDL (AlterSchema and AlterOptions) should be recovered
                    //   from Manifest on start.
                }
            }
        }

        info!(
            "Instance replay table log entries end, table:{}, table_id:{:?}, last_sequence:{}",
            table_data.name, table_data.id, last_sequence
        );

        table_data.set_last_sequence(last_sequence);

        Ok(())
    }
}
