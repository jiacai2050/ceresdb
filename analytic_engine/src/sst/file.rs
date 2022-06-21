// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Sst file and storage info

use std::{
    borrow::Borrow,
    cmp,
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    fmt,
    fmt::Debug,
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use common_types::{
    bytes::Bytes,
    schema::Schema,
    time::{TimeRange, Timestamp},
    SequenceNumber,
};
use common_util::{
    define_result,
    metric::Meter,
    runtime::{JoinHandle, Runtime},
};
use log::{debug, error, info};
use object_store::ObjectStore;
use proto::{
    common::TimeRange as TimeRangePb,
    sst::{IndexValue, SstMetaData as SstMetaDataPb, TSIDs},
};
use snafu::{ResultExt, Snafu};
use table_engine::table::TableId;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    Mutex,
};

use crate::{
    space::SpaceId,
    sst::{manager::FileId, parquet::builder::IndexMap},
    table::sst_util,
};

/// Error of sst file.
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to convert time range, err:{}", source))]
    ConvertTimeRange { source: common_types::time::Error },

    #[snafu(display("Failed to convert table schema, err:{}", source))]
    ConvertTableSchema { source: common_types::schema::Error },

    #[snafu(display("Failed to join purger, err:{}", source))]
    StopPurger { source: common_util::runtime::Error },
}

define_result!(Error);

pub type Level = u16;

// TODO(yingwen): Order or split file by time range to speed up filter (even in
//  level 0).
/// Manage files of single level
pub struct LevelHandler {
    pub level: Level,
    /// All files in current level.
    files: FileHandleSet,
}

impl LevelHandler {
    pub fn new(level: u16) -> Self {
        Self {
            level,
            files: FileHandleSet::default(),
        }
    }

    #[inline]
    pub fn insert(&mut self, file: FileHandle) {
        self.files.insert(file);
    }

    pub fn latest_sst(&self) -> Option<FileHandle> {
        self.files.latest()
    }

    pub fn pick_ssts(&self, time_range: TimeRange) -> Vec<FileHandle> {
        if self.level == 0 {
            self.files.files_by_time_range(time_range)
        } else {
            Vec::new()
        }
    }

    #[inline]
    pub fn remove_ssts(&mut self, file_ids: &[FileId]) {
        self.files.remove_by_ids(file_ids);
    }

    pub fn iter_ssts(&self) -> Iter {
        let iter = self.files.file_map.values();
        Iter(iter)
    }

    #[inline]
    pub fn collect_expired(
        &self,
        expire_time: Option<Timestamp>,
        expired_files: &mut Vec<FileHandle>,
    ) {
        self.files.collect_expired(expire_time, expired_files);
    }

    #[inline]
    pub fn has_expired_sst(&self, expire_time: Option<Timestamp>) -> bool {
        self.files.has_expired_sst(expire_time)
    }
}

pub struct Iter<'a>(std::collections::btree_map::Values<'a, FileOrdKey, FileHandle>);

impl<'a> Iterator for Iter<'a> {
    type Item = &'a FileHandle;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

#[derive(Clone)]
pub struct FileHandle {
    inner: Arc<FileHandleInner>,
}

impl PartialEq for FileHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for FileHandle {}

impl Hash for FileHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}

impl FileHandle {
    pub fn new(meta: FileMeta, purge_queue: FilePurgeQueue) -> Self {
        Self {
            inner: Arc::new(FileHandleInner {
                meta,
                purge_queue,
                being_compacted: AtomicBool::new(false),
                metrics: SstMetrics::default(),
            }),
        }
    }

    #[inline]
    pub fn read_meter(&self) -> Arc<Meter> {
        self.inner.metrics.read_meter.clone()
    }

    #[inline]
    pub fn row_num(&self) -> u64 {
        self.inner.meta.meta.row_num
    }

    #[inline]
    pub fn id(&self) -> FileId {
        self.inner.meta.id
    }

    #[inline]
    pub fn id_ref(&self) -> &FileId {
        &self.inner.meta.id
    }

    #[inline]
    pub fn intersect_with_time_range(&self, time_range: TimeRange) -> bool {
        self.inner.meta.intersect_with_time_range(time_range)
    }

    #[inline]
    pub fn min_key(&self) -> Bytes {
        self.inner.meta.meta.min_key.clone()
    }

    #[inline]
    pub fn max_key(&self) -> Bytes {
        self.inner.meta.meta.max_key.clone()
    }

    #[inline]
    pub fn time_range(&self) -> TimeRange {
        self.inner.meta.meta.time_range
    }

    #[inline]
    pub fn time_range_ref(&self) -> &TimeRange {
        &self.inner.meta.meta.time_range
    }

    #[inline]
    pub fn max_sequence(&self) -> SequenceNumber {
        self.inner.meta.meta.max_sequence
    }

    #[inline]
    pub fn being_compacted(&self) -> bool {
        self.inner.being_compacted.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn size(&self) -> u64 {
        self.inner.meta.meta.size
    }

    #[inline]
    pub fn set_being_compacted(&self, value: bool) {
        self.inner.being_compacted.store(value, Ordering::Relaxed);
    }
}

impl fmt::Debug for FileHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileHandle")
            .field("meta", &self.inner.meta)
            .field("being_compacted", &self.being_compacted())
            .field("metrics", &self.inner.metrics)
            .finish()
    }
}

struct SstMetrics {
    pub read_meter: Arc<Meter>,
    pub key_num: usize,
}

impl Default for SstMetrics {
    fn default() -> Self {
        SstMetrics {
            read_meter: Arc::new(Meter::new()),
            key_num: 0,
        }
    }
}

impl fmt::Debug for SstMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SstMetrics")
            .field("read_meter", &self.read_meter.h2_rate())
            .field("key_num", &self.key_num)
            .finish()
    }
}

struct FileHandleInner {
    meta: FileMeta,
    purge_queue: FilePurgeQueue,
    /// The file is being compacting.
    being_compacted: AtomicBool,
    metrics: SstMetrics,
}

impl Drop for FileHandleInner {
    fn drop(&mut self) {
        debug!("FileHandle is dropped, meta:{:?}", self.meta);

        // Push file cannot block or be async because we are in drop().
        self.purge_queue.push_file(self.meta.id);
    }
}

/// Used to order [FileHandle] by (end_time, start_time, file_id)
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct FileOrdKey {
    exclusive_end: Timestamp,
    inclusive_start: Timestamp,
    file_id: FileId,
}

impl FileOrdKey {
    fn for_seek(exclusive_end: Timestamp) -> Self {
        Self {
            exclusive_end,
            inclusive_start: Timestamp::MIN,
            file_id: 0,
        }
    }

    fn key_of(file: &FileHandle) -> Self {
        Self {
            exclusive_end: file.time_range().exclusive_end(),
            inclusive_start: file.time_range().inclusive_start(),
            file_id: file.id(),
        }
    }
}

/// Used to index [FileHandle] by file_id
struct FileHandleHash(FileHandle);

impl PartialEq for FileHandleHash {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for FileHandleHash {}

impl Hash for FileHandleHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id().hash(state);
    }
}

impl Borrow<FileId> for FileHandleHash {
    #[inline]
    fn borrow(&self) -> &FileId {
        self.0.id_ref()
    }
}

#[derive(Default)]
struct FileHandleSet {
    /// Files ordered by time range and id.
    file_map: BTreeMap<FileOrdKey, FileHandle>,
    /// Files indexed by file id, used to speed up removal.
    id_to_files: HashSet<FileHandleHash>,
}

impl FileHandleSet {
    fn latest(&self) -> Option<FileHandle> {
        if let Some(file) = self.file_map.values().rev().next() {
            return Some(file.clone());
        }
        None
    }

    fn files_by_time_range(&self, time_range: TimeRange) -> Vec<FileHandle> {
        // Seek to first sst whose end time >= time_range.inclusive_start().
        let seek_key = FileOrdKey::for_seek(time_range.inclusive_start());
        self.file_map
            .range(seek_key..)
            .into_iter()
            .filter_map(|(_key, file)| {
                if file.intersect_with_time_range(time_range) {
                    Some(file.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn insert(&mut self, file: FileHandle) {
        self.file_map
            .insert(FileOrdKey::key_of(&file), file.clone());
        self.id_to_files.insert(FileHandleHash(file));
    }

    fn remove_by_ids(&mut self, file_ids: &[FileId]) {
        for file_id in file_ids {
            if let Some(file) = self.id_to_files.take(file_id) {
                let key = FileOrdKey::key_of(&file.0);
                self.file_map.remove(&key);
            }
        }
    }

    /// Collect ssts with time range is expired.
    fn collect_expired(&self, expire_time: Option<Timestamp>, expired_files: &mut Vec<FileHandle>) {
        for file in self.file_map.values() {
            if file.time_range().is_expired(expire_time) {
                expired_files.push(file.clone());
            } else {
                // Files are sorted by end time first, so there is no more file whose end time
                // is less than `expire_time`.
                break;
            }
        }
    }

    fn has_expired_sst(&self, expire_time: Option<Timestamp>) -> bool {
        // Files are sorted by end time first, so check first file is enough.
        if let Some(file) = self.file_map.values().next() {
            return file.time_range().is_expired(expire_time);
        }

        false
    }
}

/// Meta of a sst file, immutable once created
#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    /// Id of the sst file
    pub id: FileId,
    pub meta: SstMetaData,
}

impl FileMeta {
    pub fn intersect_with_time_range(&self, time_range: TimeRange) -> bool {
        self.meta.time_range.intersect_with(time_range)
    }
}

/// Meta data of a sst file, immutable once created
#[derive(Debug, Clone, PartialEq)]
pub struct SstMetaData {
    pub min_key: Bytes,
    pub max_key: Bytes,
    /// Time Range of the sst
    pub time_range: TimeRange,
    /// Max sequence number in the sst
    pub max_sequence: SequenceNumber,
    pub schema: Schema,
    /// file size in bytes
    pub size: u64,
    // total row number
    pub row_num: u64,

    pub index_map: IndexMap,
}

impl From<SstMetaData> for SstMetaDataPb {
    fn from(src: SstMetaData) -> Self {
        let mut target = SstMetaDataPb::default();
        target.set_min_key(src.min_key.to_vec());
        target.set_max_key(src.max_key.to_vec());
        target.set_max_sequence(src.max_sequence);
        let time_range = TimeRangePb::from(src.time_range);
        target.set_time_range(time_range);
        target.set_schema(src.schema.into());
        target.set_size(src.size);
        target.set_row_num(src.row_num);

        src.index_map.into_iter().for_each(|(key, value)| {
            let mut index_value = IndexValue::default();
            value.into_iter().for_each(|(tag_value, tsids)| {
                let mut tsids_pb = TSIDs::default();
                tsids_pb.tsids = tsids;
                index_value.value.insert(tag_value, tsids_pb);
            });
            target.index_map.insert(key, index_value);
        });

        target
    }
}

impl TryFrom<SstMetaDataPb> for SstMetaData {
    type Error = Error;

    fn try_from(mut src: SstMetaDataPb) -> Result<Self> {
        let time_range = TimeRange::try_from(src.take_time_range()).context(ConvertTimeRange)?;
        let schema = Schema::try_from(src.take_schema()).context(ConvertTableSchema)?;
        let mut index_map = HashMap::new();
        src.index_map.into_iter().for_each(|(key, value)| {
            let mut index_value = HashMap::new();
            value.value.into_iter().for_each(|(tag_value, tsids)| {
                index_value.insert(tag_value, tsids.tsids);
            });
            index_map.insert(key, index_value);
        });
        Ok(Self {
            min_key: src.min_key.into(),
            max_key: src.max_key.into(),
            time_range,
            max_sequence: src.max_sequence,
            schema,
            size: src.size,
            row_num: src.row_num,
            index_map,
        })
    }
}

// Queue to store files to be deleted for a table.
#[derive(Clone)]
pub struct FilePurgeQueue {
    // Wrap a inner struct to avoid storing space/table ids for each file.
    inner: Arc<FilePurgeQueueInner>,
}

impl FilePurgeQueue {
    pub fn new(space_id: SpaceId, table_id: TableId, sender: UnboundedSender<Request>) -> Self {
        Self {
            inner: Arc::new(FilePurgeQueueInner {
                space_id,
                table_id,
                sender,
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Close the purge queue, then all request pushed to this queue will be
    /// ignored. This is mainly used to avoid files being deleted after the
    /// db is closed.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
    }

    fn push_file(&self, file_id: FileId) {
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }

        // Send the file id via a channel to file purger and delete the file from sst
        // store in background.
        let request = FilePurgeRequest {
            space_id: self.inner.space_id,
            table_id: self.inner.table_id,
            file_id,
        };

        if let Err(send_res) = self.inner.sender.send(Request::Purge(request)) {
            error!(
                "Failed to send delete file request, request:{:?}",
                send_res.0
            );
        }
    }
}

struct FilePurgeQueueInner {
    space_id: SpaceId,
    table_id: TableId,
    closed: AtomicBool,
    sender: UnboundedSender<Request>,
}

#[derive(Debug)]
pub struct FilePurgeRequest {
    space_id: SpaceId,
    table_id: TableId,
    file_id: FileId,
}

#[derive(Debug)]
pub enum Request {
    Purge(FilePurgeRequest),
    Exit,
}

/// Background file purger.
pub struct FilePurger {
    sender: UnboundedSender<Request>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl FilePurger {
    pub fn start<Store: ObjectStore + Send + Sync + 'static>(
        runtime: &Runtime,
        store: Arc<Store>,
    ) -> Self {
        // We must use unbound channel, so the sender wont block when the handle is
        // dropped.
        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn a background job to purge files.
        let handle = runtime.spawn(async {
            Self::purge_file_loop(store, rx).await;
        });

        Self {
            sender: tx,
            handle: Mutex::new(Some(handle)),
        }
    }

    pub async fn stop(&self) -> Result<()> {
        info!("Try to stop file purger");

        if self.sender.send(Request::Exit).is_err() {
            error!("File purge task already exited");
        }

        let mut handle = self.handle.lock().await;
        // Also clear the handle to avoid await a ready future.
        if let Some(h) = handle.take() {
            h.await.context(StopPurger)?;
        }

        Ok(())
    }

    pub fn create_purge_queue(&self, space_id: SpaceId, table_id: TableId) -> FilePurgeQueue {
        FilePurgeQueue::new(space_id, table_id, self.sender.clone())
    }

    async fn purge_file_loop<Store: ObjectStore>(
        store: Arc<Store>,
        mut receiver: UnboundedReceiver<Request>,
    ) {
        info!("File purger start");

        while let Some(request) = receiver.recv().await {
            match request {
                Request::Purge(purge_request) => {
                    let sst_file_path = sst_util::new_sst_file_path(
                        purge_request.space_id,
                        purge_request.table_id,
                        purge_request.file_id,
                    );

                    info!(
                        "File purger delete file, purge_request:{:?}, sst_file_path:{}",
                        purge_request,
                        sst_file_path.to_string()
                    );

                    if let Err(e) = store.delete(&sst_file_path).await {
                        error!(
                            "File purger failed to delete file, sst_file_path:{}, err:{}",
                            sst_file_path.to_string(),
                            e
                        );
                    }
                }
                Request::Exit => break,
            }
        }

        info!("File purger exit");
    }
}

/// Merge sst meta of given `files`, panic if `files` is empty.
///
/// The size and row_num of the merged meta is initialized to 0.
pub fn merge_sst_meta(files: &[FileHandle], schema: Schema) -> SstMetaData {
    let mut min_key = files[0].min_key();
    let mut max_key = files[0].max_key();
    let mut time_range_start = files[0].time_range().inclusive_start();
    let mut time_range_end = files[0].time_range().exclusive_end();
    let mut max_sequence = files[0].max_sequence();

    if files.len() > 1 {
        for file in &files[1..] {
            min_key = cmp::min(file.min_key(), min_key);
            max_key = cmp::max(file.max_key(), max_key);
            time_range_start = cmp::min(file.time_range().inclusive_start(), time_range_start);
            time_range_end = cmp::max(file.time_range().exclusive_end(), time_range_end);
            max_sequence = cmp::max(file.max_sequence(), max_sequence);
        }
    }

    SstMetaData {
        min_key,
        max_key,
        time_range: TimeRange::new(time_range_start, time_range_end).unwrap(),
        max_sequence,
        schema,
        // we don't know file size and total row number yet
        size: 0,
        row_num: 0,
        index_map: HashMap::new(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub struct FilePurgerMocker;

    impl FilePurgerMocker {
        pub fn mock() -> FilePurger {
            let (sender, _receiver) = mpsc::unbounded_channel();

            FilePurger {
                sender,
                handle: Mutex::new(None),
            }
        }
    }

    #[must_use]
    pub struct SstMetaDataMocker {
        schema: Schema,
        time_range: TimeRange,
        max_sequence: SequenceNumber,
    }

    impl SstMetaDataMocker {
        pub fn new(schema: Schema) -> Self {
            Self {
                schema,
                time_range: TimeRange::min_to_max(),
                max_sequence: 1,
            }
        }

        pub fn time_range(mut self, range: TimeRange) -> Self {
            self.time_range = range;
            self
        }

        pub fn max_sequence(mut self, max_sequence: SequenceNumber) -> Self {
            self.max_sequence = max_sequence;
            self
        }

        pub fn build(&self) -> SstMetaData {
            SstMetaData {
                min_key: Bytes::new(),
                max_key: Bytes::new(),
                time_range: self.time_range,
                max_sequence: self.max_sequence,
                schema: self.schema.clone(),
                size: 0,
                row_num: 0,
            }
        }
    }
}
