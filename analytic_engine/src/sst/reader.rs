// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Sst reader trait definition.

use async_trait::async_trait;
use common_types::record_batch::RecordBatchWithKey;
use futures::Stream;

use crate::sst::file::SstMetaData;

pub mod error {
    use common_util::define_result;
    use snafu::{Backtrace, Snafu};

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("Try to read again, path:{}.\nBacktrace:\n{}", path, backtrace))]
        ReadAgain { backtrace: Backtrace, path: String },

        #[snafu(display("Fail to read persisted file, path:{}, err:{}", path, source))]
        ReadPersist {
            path: String,
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[snafu(display("Failed to decode record batch, err:{}", source))]
        DecodeRecordBatch {
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[snafu(display("Failed to decode sst meta data, err:{}", source))]
        DecodeSstMeta {
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[snafu(display("Sst meta data is not found.\nBacktrace:\n{}", backtrace))]
        SstMetaNotFound { backtrace: Backtrace },

        #[snafu(display("Fail to projection, err:{}", source))]
        Projection {
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[snafu(display("Sst meta data is empty.\nBacktrace:\n{}", backtrace))]
        EmptySstMeta { backtrace: Backtrace },

        #[snafu(display("Invalid schema, err:{}", source))]
        InvalidSchema { source: common_types::schema::Error },

        #[snafu(display("datafusion error:{}", source))]
        DataFusionError {
            source: datafusion::error::DataFusionError,
        },

        #[snafu(display("Other kind of error:{}", source))]
        Other {
            source: Box<dyn std::error::Error + Send + Sync>,
        },
    }

    define_result!(Error);
}

pub use error::*;

#[async_trait]
pub trait SstReader {
    async fn meta_data(&mut self) -> Result<&SstMetaData>;

    async fn read(
        &mut self,
    ) -> Result<Box<dyn Stream<Item = Result<RecordBatchWithKey>> + Send + Unpin>>;
}

#[cfg(test)]
pub mod tests {
    use common_types::row::Row;
    use futures::StreamExt;

    use super::*;

    pub async fn check_stream<S>(stream: &mut S, expected_rows: Vec<Row>)
    where
        S: Stream<Item = Result<RecordBatchWithKey>> + Unpin,
    {
        let mut visited_rows = 0;
        while let Some(batch) = stream.next().await {
            let batch = batch.unwrap();
            for row_idx in 0..batch.num_rows() {
                assert_eq!(batch.clone_row_at(row_idx), expected_rows[visited_rows]);
                visited_rows += 1;
            }
        }

        assert_eq!(visited_rows, expected_rows.len());
    }
}
