// Copyright 2023 The CeresDB Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Test utils.

use std::{collections::HashMap, future::Future, sync::Arc};

use common_types::{
    datum::Datum,
    record_batch::RecordBatch,
    row::{Row, RowGroup},
    table::{ShardId, DEFAULT_SHARD_ID},
    time::Timestamp,
};
use futures::stream::StreamExt;
use logger::info;
use object_store::config::{LocalOptions, ObjectStoreOptions, StorageOptions};
use size_ext::ReadableSize;
use table_engine::{
    engine::{
        CreateTableRequest, DropTableRequest, EngineRuntimes, OpenShardRequest, OpenTableRequest,
        Result as EngineResult, TableDef, TableEngineRef,
    },
    table::{
        AlterSchemaRequest, FlushRequest, GetRequest, ReadRequest, Result, SchemaId, TableId,
        TableRef, WriteRequest,
    },
};
use tempfile::TempDir;
use time_ext::ReadableDuration;

use crate::{
    setup::{EngineBuilder, MemWalsOpener, OpenedWals, RocksDBWalsOpener, WalsOpener},
    tests::table::{self, FixedSchemaTable, RowTuple},
    Config, DynamicConfig, RecoverMode, RocksDBConfig, WalStorageConfig,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Helper struct to create a null datum.
pub struct Null;

impl From<Null> for Datum {
    fn from(_data: Null) -> Datum {
        Datum::Null
    }
}

pub async fn check_read<T: WalsOpener>(
    test_ctx: &TestContext<T>,
    fixed_schema_table: &FixedSchemaTable,
    msg: &str,
    table_name: &str,
    rows: &[RowTuple<'_>],
) {
    for read_opts in table::read_opts_list() {
        info!("{}, opts:{:?}", msg, read_opts);

        let record_batches = test_ctx
            .read_table(
                table_name,
                fixed_schema_table.new_read_all_request(read_opts),
            )
            .await;

        fixed_schema_table.assert_batch_eq_to_rows(&record_batches, rows);
    }
}

pub async fn check_get<T: WalsOpener>(
    test_ctx: &TestContext<T>,
    fixed_schema_table: &FixedSchemaTable,
    msg: &str,
    table_name: &str,
    rows: &[RowTuple<'_>],
) {
    for row_data in rows {
        let request = fixed_schema_table.new_get_request_from_row(*row_data);

        info!("{}, request:{:?}, row_data:{:?}", msg, request, row_data);

        let row = test_ctx.get_from_table(table_name, request).await.unwrap();

        fixed_schema_table.assert_row_eq(*row_data, row);
    }
}

pub struct TestContext<T> {
    config: Config,
    wals_opener: T,
    runtimes: Arc<EngineRuntimes>,
    engine: Option<TableEngineRef>,
    opened_wals: Option<OpenedWals>,
    schema_id: SchemaId,
    last_table_seq: u32,
    open_method: OpenTablesMethod,

    name_to_tables: HashMap<String, TableRef>,
}

impl<T: WalsOpener> TestContext<T> {
    pub async fn open(&mut self) {
        let opened_wals = if let Some(opened_wals) = self.opened_wals.take() {
            opened_wals
        } else {
            self.wals_opener
                .open_wals(&self.config.wal, self.runtimes.clone())
                .await
                .unwrap()
        };
        let dynamic_config = Arc::new(DynamicConfig::default());
        let engine_builder = EngineBuilder {
            config: &self.config,
            dynamic_config: &dynamic_config,
            engine_runtimes: self.runtimes.clone(),
            opened_wals: opened_wals.clone(),
        };
        self.opened_wals = Some(opened_wals);
        self.engine = Some(engine_builder.build().await.unwrap());
    }

    pub async fn reopen(&mut self) {
        {
            // Close all tables.
            self.name_to_tables.clear();

            // Close engine.
            let engine = self.engine.take().unwrap();
            engine.close().await.unwrap();
        }

        self.open().await;
    }

    pub async fn reopen_with_tables(&mut self, tables: &[&str]) {
        let table_infos: Vec<_> = tables
            .iter()
            .map(|name| {
                let table_id = self.name_to_tables.get(*name).unwrap().id();
                (table_id, *name)
            })
            .collect();
        {
            // Close all tables.
            self.name_to_tables.clear();

            // Close engine.
            let engine = self.engine.take().unwrap();
            engine.close().await.unwrap();
        }

        self.open().await;

        match self.open_method {
            OpenTablesMethod::WithOpenTable => {
                for (id, name) in table_infos {
                    self.open_table(id, name).await;
                }
            }
            OpenTablesMethod::WithOpenShard => {
                self.open_tables_of_shard(table_infos, DEFAULT_SHARD_ID)
                    .await;
            }
        }
    }

    pub async fn reopen_with_tables_of_shard(&mut self, tables: &[&str], shard_id: ShardId) {
        let table_infos: Vec<_> = tables
            .iter()
            .map(|name| {
                let table_id = self.name_to_tables.get(*name).unwrap().id();
                (table_id, *name)
            })
            .collect();
        {
            // Close all tables.
            self.name_to_tables.clear();

            // Close engine.
            let engine = self.engine.take().unwrap();
            engine.close().await.unwrap();
        }

        self.open().await;

        self.open_tables_of_shard(table_infos, shard_id).await
    }

    async fn open_tables_of_shard(&mut self, table_infos: Vec<(TableId, &str)>, shard_id: ShardId) {
        let table_defs = table_infos
            .into_iter()
            .map(|table| TableDef {
                catalog_name: "ceresdb".to_string(),
                schema_name: "public".to_string(),
                schema_id: self.schema_id,
                id: table.0,
                name: table.1.to_string(),
            })
            .collect();

        let open_shard_request = OpenShardRequest {
            shard_id,
            table_defs,
            engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
        };

        let tables = self
            .engine()
            .open_shard(open_shard_request)
            .await
            .unwrap()
            .into_values()
            .map(|result| result.unwrap().unwrap());

        for table in tables {
            self.name_to_tables.insert(table.name().to_string(), table);
        }
    }

    async fn open_table(&mut self, table_id: TableId, table_name: &str) {
        let table = self
            .engine()
            .open_table(OpenTableRequest {
                catalog_name: "ceresdb".to_string(),
                schema_name: "public".to_string(),
                schema_id: self.schema_id,
                table_name: table_name.to_string(),
                table_id,
                engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
                shard_id: DEFAULT_SHARD_ID,
            })
            .await
            .unwrap()
            .unwrap();

        self.name_to_tables.insert(table_name.to_string(), table);
    }

    pub async fn try_open_table(
        &mut self,
        table_id: TableId,
        table_name: &str,
    ) -> EngineResult<Option<TableRef>> {
        let table_opt = self
            .engine()
            .open_table(OpenTableRequest {
                catalog_name: "ceresdb".to_string(),
                schema_name: "public".to_string(),
                schema_id: self.schema_id,
                table_name: table_name.to_string(),
                table_id,
                engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
                shard_id: DEFAULT_SHARD_ID,
            })
            .await?;

        let table = match table_opt {
            Some(v) => v,
            None => return Ok(None),
        };

        self.name_to_tables
            .insert(table_name.to_string(), table.clone());

        Ok(Some(table))
    }

    pub async fn drop_table(&mut self, table_name: &str) -> bool {
        let request = DropTableRequest {
            catalog_name: "ceresdb".to_string(),
            schema_name: "public".to_string(),
            schema_id: self.schema_id,
            table_name: table_name.to_string(),
            engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
        };

        let ret = self.engine().drop_table(request).await.unwrap();

        self.name_to_tables.remove(table_name);

        ret
    }

    /// 3 days ago.
    pub fn start_ms(&self) -> i64 {
        Timestamp::now().as_i64() - 3 * DAY_MS
    }

    pub async fn create_fixed_schema_table(&mut self, table_name: &str) -> FixedSchemaTable {
        let fixed_schema_table = FixedSchemaTable::builder()
            .schema_id(self.schema_id)
            .table_name(table_name.to_string())
            .table_id(self.next_table_id())
            .ttl("7d".parse::<ReadableDuration>().unwrap())
            .build_fixed();

        self.create_table(fixed_schema_table.create_request().clone())
            .await;

        fixed_schema_table
    }

    async fn create_table(&mut self, create_request: CreateTableRequest) {
        let table_name = create_request.params.table_name.clone();
        let table = self.engine().create_table(create_request).await.unwrap();

        self.name_to_tables.insert(table_name.to_string(), table);
    }

    pub async fn write_to_table(&self, table_name: &str, row_group: RowGroup) {
        let table = self.table(table_name);

        table.write(WriteRequest { row_group }).await.unwrap();
    }

    pub async fn read_table(
        &self,
        table_name: &str,
        read_request: ReadRequest,
    ) -> Vec<RecordBatch> {
        let table = self.table(table_name);

        let mut stream = table.read(read_request).await.unwrap();
        let mut record_batches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.unwrap();

            record_batches.push(batch);
        }

        record_batches
    }

    pub async fn partitioned_read_table(
        &self,
        table_name: &str,
        read_request: ReadRequest,
    ) -> Vec<RecordBatch> {
        let table = self.table(table_name);

        let streams = table.partitioned_read(read_request).await.unwrap();
        let mut record_batches = Vec::new();

        for mut stream in streams.streams {
            while let Some(batch) = stream.next().await {
                let batch = batch.unwrap();

                record_batches.push(batch);
            }
        }

        record_batches
    }

    pub async fn get_from_table(&self, table_name: &str, request: GetRequest) -> Option<Row> {
        let table = self.table(table_name);

        table.get(request).await.unwrap()
    }

    pub async fn flush_table(&self, table_name: &str) {
        let table = self.table(table_name);

        table.flush(FlushRequest::default()).await.unwrap();
    }

    pub async fn flush_table_with_request(&self, table_name: &str, request: FlushRequest) {
        let table = self.table(table_name);

        table.flush(request).await.unwrap();
    }

    pub async fn compact_table(&self, table_name: &str) {
        let table = self.table(table_name);

        table.compact().await.unwrap();
    }

    pub async fn try_alter_schema(
        &self,
        table_name: &str,
        request: AlterSchemaRequest,
    ) -> Result<usize> {
        let table = self.table(table_name);

        table.alter_schema(request).await
    }

    pub async fn try_alter_options(
        &self,
        table_name: &str,
        opts: HashMap<String, String>,
    ) -> Result<usize> {
        let table = self.table(table_name);

        table.alter_options(opts).await
    }

    pub fn table(&self, table_name: &str) -> TableRef {
        self.name_to_tables.get(table_name).cloned().unwrap()
    }

    #[inline]
    pub fn engine(&self) -> &TableEngineRef {
        self.engine.as_ref().unwrap()
    }

    fn next_table_id(&mut self) -> TableId {
        self.last_table_seq += 1;
        table::new_table_id(2, self.last_table_seq)
    }
}

#[derive(Clone, Copy)]
pub enum OpenTablesMethod {
    WithOpenTable,
    WithOpenShard,
}

impl<T> TestContext<T> {
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn clone_engine(&self) -> TableEngineRef {
        self.engine.clone().unwrap()
    }
}

pub struct TestEnv {
    _dir: TempDir,
    pub config: Config,
    pub runtimes: Arc<EngineRuntimes>,
}

impl TestEnv {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn new_context<T: EngineBuildContext>(
        &self,
        build_context: T,
    ) -> TestContext<T::WalsOpener> {
        let config = build_context.config();
        let wals_opener = build_context.wals_opener();

        TestContext {
            config,
            wals_opener,
            runtimes: self.runtimes.clone(),
            engine: None,
            opened_wals: None,
            schema_id: SchemaId::from_u32(100),
            last_table_seq: 1,
            name_to_tables: HashMap::new(),
            open_method: build_context.open_method(),
        }
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtimes.default_runtime.block_on(future)
    }
}

pub struct Builder {
    num_workers: usize,
}

impl Builder {
    pub fn build(self) -> TestEnv {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                disk_cache_dir: "".to_string(),
                disk_cache_capacity: ReadableSize::mb(0),
                disk_cache_page_size: ReadableSize::mb(0),
                disk_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_dir: dir.path().to_str().unwrap().to_string(),
                }),
            },
            wal: WalStorageConfig::RocksDB(Box::new(RocksDBConfig {
                data_dir: dir.path().to_str().unwrap().to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let runtime = Arc::new(
            runtime::Builder::default()
                .worker_threads(self.num_workers)
                .enable_all()
                .build()
                .unwrap(),
        );

        TestEnv {
            _dir: dir,
            config,
            runtimes: Arc::new(EngineRuntimes {
                read_runtime: runtime.clone(),
                write_runtime: runtime.clone(),
                meta_runtime: runtime.clone(),
                compact_runtime: runtime.clone(),
                default_runtime: runtime.clone(),
                io_runtime: runtime,
            }),
        }
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self { num_workers: 2 }
    }
}

pub trait EngineBuildContext: Clone + Default {
    type WalsOpener: WalsOpener;

    fn wals_opener(&self) -> Self::WalsOpener;
    fn config(&self) -> Config;
    fn open_method(&self) -> OpenTablesMethod;
}

pub struct RocksDBEngineBuildContext {
    config: Config,
    open_method: OpenTablesMethod,
}

impl RocksDBEngineBuildContext {
    pub fn new(mode: RecoverMode, open_method: OpenTablesMethod) -> Self {
        let mut context = Self::default();
        context.config.recover_mode = mode;
        context.open_method = open_method;

        context
    }
}

impl Default for RocksDBEngineBuildContext {
    fn default() -> Self {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                disk_cache_dir: "".to_string(),
                disk_cache_capacity: ReadableSize::mb(0),
                disk_cache_page_size: ReadableSize::mb(0),
                disk_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_dir: dir.path().to_str().unwrap().to_string(),
                }),
            },

            wal: WalStorageConfig::RocksDB(Box::new(RocksDBConfig {
                data_dir: dir.path().to_str().unwrap().to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };

        Self {
            config,
            open_method: OpenTablesMethod::WithOpenTable,
        }
    }
}

impl Clone for RocksDBEngineBuildContext {
    fn clone(&self) -> Self {
        let mut config = self.config.clone();

        let dir = tempfile::tempdir().unwrap();
        let storage = StorageOptions {
            mem_cache_capacity: ReadableSize::mb(0),
            mem_cache_partition_bits: 0,
            disk_cache_dir: "".to_string(),
            disk_cache_capacity: ReadableSize::mb(0),
            disk_cache_page_size: ReadableSize::mb(0),
            disk_cache_partition_bits: 0,
            object_store: ObjectStoreOptions::Local(LocalOptions {
                data_dir: dir.path().to_str().unwrap().to_string(),
            }),
        };

        config.storage = storage;
        config.wal = WalStorageConfig::RocksDB(Box::new(RocksDBConfig {
            data_dir: dir.path().to_str().unwrap().to_string(),
            ..Default::default()
        }));

        Self {
            config,
            open_method: self.open_method,
        }
    }
}

impl EngineBuildContext for RocksDBEngineBuildContext {
    type WalsOpener = RocksDBWalsOpener;

    fn wals_opener(&self) -> Self::WalsOpener {
        RocksDBWalsOpener
    }

    fn config(&self) -> Config {
        self.config.clone()
    }

    fn open_method(&self) -> OpenTablesMethod {
        self.open_method
    }
}

#[derive(Clone)]
pub struct MemoryEngineBuildContext {
    config: Config,
    open_method: OpenTablesMethod,
}

impl MemoryEngineBuildContext {
    pub fn new(mode: RecoverMode, open_method: OpenTablesMethod) -> Self {
        let mut context = Self::default();
        context.config.recover_mode = mode;
        context.open_method = open_method;

        context
    }
}

impl Default for MemoryEngineBuildContext {
    fn default() -> Self {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                disk_cache_dir: "".to_string(),
                disk_cache_capacity: ReadableSize::mb(0),
                disk_cache_page_size: ReadableSize::mb(0),
                disk_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_dir: dir.path().to_str().unwrap().to_string(),
                }),
            },
            wal: WalStorageConfig::Obkv(Box::default()),
            ..Default::default()
        };

        Self {
            config,
            open_method: OpenTablesMethod::WithOpenTable,
        }
    }
}

impl EngineBuildContext for MemoryEngineBuildContext {
    type WalsOpener = MemWalsOpener;

    fn wals_opener(&self) -> Self::WalsOpener {
        MemWalsOpener::default()
    }

    fn config(&self) -> Config {
        self.config.clone()
    }

    fn open_method(&self) -> OpenTablesMethod {
        self.open_method
    }
}

pub fn rocksdb_ctxs() -> Vec<RocksDBEngineBuildContext> {
    vec![
        RocksDBEngineBuildContext::new(RecoverMode::TableBased, OpenTablesMethod::WithOpenTable),
        RocksDBEngineBuildContext::new(RecoverMode::ShardBased, OpenTablesMethod::WithOpenTable),
        RocksDBEngineBuildContext::new(RecoverMode::TableBased, OpenTablesMethod::WithOpenShard),
        RocksDBEngineBuildContext::new(RecoverMode::ShardBased, OpenTablesMethod::WithOpenShard),
    ]
}

pub fn memory_ctxs() -> Vec<MemoryEngineBuildContext> {
    vec![
        MemoryEngineBuildContext::new(RecoverMode::TableBased, OpenTablesMethod::WithOpenTable),
        MemoryEngineBuildContext::new(RecoverMode::ShardBased, OpenTablesMethod::WithOpenTable),
        MemoryEngineBuildContext::new(RecoverMode::TableBased, OpenTablesMethod::WithOpenShard),
        MemoryEngineBuildContext::new(RecoverMode::ShardBased, OpenTablesMethod::WithOpenShard),
    ]
}
