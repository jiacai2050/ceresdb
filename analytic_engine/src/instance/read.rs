// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Read logic of instance

use std::{
    collections::BTreeMap,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use common_types::{
    projected_schema::ProjectedSchema, record_batch::RecordBatch, schema::RecordSchema,
    time::TimeRange,
};
use common_util::{define_result, runtime::Runtime, time::InstantExt};
use futures::stream::Stream;
use log::{debug, error, trace};
use snafu::{ResultExt, Snafu};
use table_engine::{
    stream::{
        self, ErrWithSource, PartitionedStreams, RecordBatchStream, SendableRecordBatchStream,
    },
    table::ReadRequest,
};
use tokio::sync::mpsc::{self, Receiver};

use crate::{
    instance::Instance,
    row_iter::{
        chain,
        chain::{ChainConfig, ChainIterator},
        dedup::DedupIterator,
        merge::{MergeBuilder, MergeConfig, MergeIterator},
        IterOptions, RecordBatchWithKeyIterator,
    },
    space::SpaceAndTable,
    sst::factory::SstReaderOptions,
    table::{
        data::TableData,
        version::{ReadView, TableVersion},
    },
    table_options::TableOptions,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to scan memtable, table:{}, err:{}", table, source))]
    ScanMemTable {
        table: String,
        source: crate::memtable::Error,
    },

    #[snafu(display("Failed to build merge iterator, table:{}, err:{}", table, source))]
    BuildMergeIterator {
        table: String,
        source: crate::row_iter::merge::Error,
    },

    #[snafu(display("Failed to build chain iterator, table:{}, err:{}", table, source))]
    BuildChainIterator {
        table: String,
        source: crate::row_iter::chain::Error,
    },
}

define_result!(Error);

const RECORD_BATCH_READ_BUF_SIZE: usize = 1000;

/// Check whether it needs to apply merge sorting when reading the table with
/// the `table_options` by the `read_request`.
fn need_merge_sort_streams(table_options: &TableOptions, read_request: &ReadRequest) -> bool {
    table_options.need_dedup() || read_request.order.is_in_order()
}

impl Instance {
    /// Read data in multiple time range from table, and return
    /// `read_parallelism` output streams.
    pub async fn partitioned_read_from_table(
        &self,
        space_table: &SpaceAndTable,
        request: ReadRequest,
    ) -> Result<PartitionedStreams> {
        debug!(
            "Instance read from table, space_id:{}, table:{}, table_id:{:?}, request:{:?}",
            space_table.space().id,
            space_table.table_data().name,
            space_table.table_data().id,
            request
        );

        let table_data = space_table.table_data();

        // Collect metrics.
        table_data.metrics.on_read_request_begin();

        let iter_options = IterOptions::new(self.scan_batch_size);
        let table_options = table_data.table_options();

        if need_merge_sort_streams(&table_data.table_options(), &request) {
            let merge_iters = self
                .build_merge_iters(table_data, &request, iter_options, &table_options)
                .await?;
            self.build_partitioned_streams(&request, merge_iters)
        } else {
            let chain_iters = self
                .build_chain_iters(table_data, &request, &table_options)
                .await?;
            self.build_partitioned_streams(&request, chain_iters)
        }
    }

    fn build_partitioned_streams(
        &self,
        request: &ReadRequest,
        mut partitioned_iters: Vec<impl RecordBatchWithKeyIterator + 'static>,
    ) -> Result<PartitionedStreams> {
        let read_parallelism = request.opts.read_parallelism;

        if read_parallelism == 1 && request.order.is_in_desc_order() {
            // TODO(xikai): it seems this can be avoided.
            partitioned_iters.reverse();
        };

        // Split iterators into `read_parallelism` groups.
        let mut splited_iters: Vec<_> = std::iter::repeat_with(Vec::new)
            .take(read_parallelism)
            .collect();

        for (i, time_aligned_iter) in partitioned_iters.into_iter().enumerate() {
            splited_iters[i % read_parallelism].push(time_aligned_iter);
        }

        let mut streams = Vec::with_capacity(read_parallelism);
        for iters in splited_iters {
            let stream = iters_to_stream(iters, self.read_runtime(), &request.projected_schema);
            streams.push(stream);
        }

        assert_eq!(read_parallelism, streams.len());

        Ok(PartitionedStreams { streams })
    }

    async fn build_merge_iters(
        &self,
        table_data: &TableData,
        request: &ReadRequest,
        iter_options: IterOptions,
        table_options: &TableOptions,
    ) -> Result<Vec<DedupIterator<MergeIterator>>> {
        // Current visible sequence
        let begin_instant = Instant::now();

        let sequence = table_data.last_sequence();
        let projected_schema = request.projected_schema.clone();
        let sst_reader_options = SstReaderOptions {
            sst_type: table_data.sst_type,
            read_batch_row_num: table_options.num_rows_per_row_group,
            reverse: request.order.is_in_desc_order(),
            projected_schema: projected_schema.clone(),
            predicate: request.predicate.clone(),
            meta_cache: self.meta_cache.clone(),
            data_cache: self.data_cache.clone(),
            runtime: self.read_runtime().clone(),
        };

        let time_range = request.predicate.time_range();
        let version = table_data.current_version();
        let read_views = self.partition_ssts_and_memtables(time_range, version, table_options);

        for view in &read_views {
            debug!("read_views:{:?}", view.leveled_ssts);
        }
        let mut iters = Vec::with_capacity(read_views.len());
        for read_view in read_views {
            let merge_config = MergeConfig {
                request_id: request.request_id,
                space_id: table_data.space_id,
                table_id: table_data.id,
                sequence,
                projected_schema: projected_schema.clone(),
                predicate: request.predicate.clone(),
                sst_factory: &self.space_store.sst_factory,
                sst_reader_options: sst_reader_options.clone(),
                store: self.space_store.store_ref(),
                merge_iter_options: iter_options.clone(),
                need_dedup: table_options.need_dedup(),
                reverse: request.order.is_in_desc_order(),
            };

            let merge_iter = MergeBuilder::new(merge_config)
                .sampling_mem(read_view.sampling_mem)
                .memtables(read_view.memtables)
                .ssts_of_level(read_view.leveled_ssts)
                .build()
                .await
                .context(BuildMergeIterator {
                    table: &table_data.name,
                })?;
            let dedup_iter =
                DedupIterator::new(request.request_id, merge_iter, iter_options.clone());

            iters.push(dedup_iter);
        }

        debug!(
            "build merge iter done. cost:{}ms",
            begin_instant.saturating_elapsed().as_millis(),
        );
        Ok(iters)
    }

    async fn build_chain_iters(
        &self,
        table_data: &TableData,
        request: &ReadRequest,
        table_options: &TableOptions,
    ) -> Result<Vec<ChainIterator>> {
        let projected_schema = request.projected_schema.clone();

        assert!(request.order.is_out_of_order());

        let sst_reader_options = SstReaderOptions {
            sst_type: table_data.sst_type,
            read_batch_row_num: table_options.num_rows_per_row_group,
            // no need to read in order so just read in asc order by default.
            reverse: false,
            projected_schema: projected_schema.clone(),
            predicate: request.predicate.clone(),
            meta_cache: self.meta_cache.clone(),
            data_cache: self.data_cache.clone(),
            runtime: self.read_runtime().clone(),
        };

        let time_range = request.predicate.time_range();
        let version = table_data.current_version();
        let read_views = self.partition_ssts_and_memtables(time_range, version, table_options);

        let mut iters = Vec::with_capacity(read_views.len());
        for read_view in read_views {
            let chain_config = ChainConfig {
                request_id: request.request_id,
                space_id: table_data.space_id,
                table_id: table_data.id,
                projected_schema: projected_schema.clone(),
                predicate: request.predicate.clone(),
                sst_reader_options: sst_reader_options.clone(),
                sst_factory: &self.space_store.sst_factory,
                store: self.space_store.store_ref(),
            };
            let builder = chain::Builder::new(chain_config);
            let chain_iter = builder
                .sampling_mem(read_view.sampling_mem)
                .memtables(read_view.memtables)
                .ssts(read_view.leveled_ssts)
                .build()
                .await
                .context(BuildChainIterator {
                    table: &table_data.name,
                })?;

            iters.push(chain_iter);
        }

        Ok(iters)
    }

    fn partition_ssts_and_memtables(
        &self,
        time_range: TimeRange,
        version: &TableVersion,
        table_options: &TableOptions,
    ) -> Vec<ReadView> {
        let read_view = version.pick_read_view(time_range);

        let segment_duration = match table_options.segment_duration {
            Some(v) => v.0,
            None => {
                // Segment duration is unknown, the table maybe still in sampling phase
                // or the segment duration is still not applied to the table options,
                // just return one partition.
                return vec![read_view];
            }
        };
        if read_view.contains_sampling() {
            // The table contains sampling memtable, just return one partition.
            return vec![read_view];
        }

        // Collect the aligned ssts and memtables into the map.
        // {aligned timestamp} => {read view}
        let mut read_view_by_time = BTreeMap::new();
        for (level, leveled_ssts) in read_view.leveled_ssts.into_iter().enumerate() {
            for file in leveled_ssts {
                let aligned_ts = file
                    .time_range()
                    .inclusive_start()
                    .truncate_by(segment_duration);
                let entry = read_view_by_time
                    .entry(aligned_ts)
                    .or_insert_with(ReadView::default);
                entry.leveled_ssts[level].push(file);
            }
        }

        for memtable in read_view.memtables {
            let aligned_ts = memtable
                .time_range
                .inclusive_start()
                .truncate_by(segment_duration);
            let entry = read_view_by_time
                .entry(aligned_ts)
                .or_insert_with(ReadView::default);
            entry.memtables.push(memtable);
        }

        read_view_by_time.into_values().collect()
    }
}

// TODO(xikai): this is a hack way to implement SendableRecordBatchStream for
// MergeIterator.
fn iters_to_stream<T>(
    collection: T,
    runtime: &Runtime,
    schema: &ProjectedSchema,
) -> SendableRecordBatchStream
where
    T: IntoIterator + Send + 'static,
    T::Item: RecordBatchWithKeyIterator,
    T::IntoIter: Send,
{
    let (tx, rx) = mpsc::channel(RECORD_BATCH_READ_BUF_SIZE);
    let projected_schema = schema.clone();

    runtime.spawn(async move {
        for mut iter in collection {
            while let Some(record_batch) = iter.next_batch().await.transpose() {
                let record_batch =
                    record_batch
                        .map_err(|e| Box::new(e) as _)
                        .context(ErrWithSource {
                            msg: "Read record batch",
                        });

                // Apply the projection to RecordBatchWithKey and gets the final RecordBatch.
                let record_batch = record_batch.and_then(|batch_with_key| {
                    // TODO(yingwen): Try to use projector to do this, which precompute row
                    // indexes to project.
                    batch_with_key
                        .try_project(&projected_schema)
                        .map_err(|e| Box::new(e) as _)
                        .context(ErrWithSource {
                            msg: "Project record batch",
                        })
                });

                trace!("send next record batch:{:?}", record_batch);
                if tx.send(record_batch).await.is_err() {
                    error!("Failed to send record batch from the merge iterator");
                    break;
                }
            }
        }
    });

    Box::pin(ChannelledRecordBatchStream {
        schema: schema.to_record_schema(),
        rx,
    })
}

pub struct ChannelledRecordBatchStream {
    schema: RecordSchema,
    rx: Receiver<stream::Result<RecordBatch>>,
}

impl Stream for ChannelledRecordBatchStream {
    type Item = stream::Result<RecordBatch>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.rx).poll_recv(cx)
    }
}

impl RecordBatchStream for ChannelledRecordBatchStream {
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}
