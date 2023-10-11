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

//! Drop table logic of instance

use common_types::SequenceNumber;
use logger::{info, warn};
use snafu::ResultExt;
use table_engine::engine::DropTableRequest;

use crate::{
    instance::{
        engine::{Result, WriteManifest},
        SpaceStoreRef,
    },
    manifest::meta_edit::{DropTableMeta, MetaEdit, MetaEditRequest, MetaUpdate},
    space::SpaceRef,
};

pub(crate) struct Dropper {
    pub space: SpaceRef,
    pub space_store: SpaceStoreRef,
}

impl Dropper {
    /// Drop a table under given space
    pub async fn drop(&self, request: DropTableRequest) -> Result<bool> {
        info!("Try to drop table, request:{:?}", request);

        let table_data = match self.space.find_table(&request.table_name) {
            Some(v) => v,
            None => {
                warn!("No need to drop a dropped table, request:{:?}", request);
                return Ok(false);
            }
        };

        if table_data.is_dropped() {
            warn!(
                "Process drop table command tries to drop a dropped table, table:{:?}",
                table_data.name,
            );
            return Ok(false);
        }

        // Mark table's WAL for deletable, memtable will also get freed automatically
        // when table_data is dropped.
        let table_location = table_data.table_location();
        let wal_location =
            crate::instance::create_wal_location(table_location.id, table_location.shard_info);
        self.space_store
            .wal_manager
            .mark_delete_entries_up_to(wal_location, SequenceNumber::MAX)
            .await
            .unwrap();

        // Store the dropping information into meta
        let edit_req = {
            let meta_update = MetaUpdate::DropTable(DropTableMeta {
                space_id: self.space.id,
                table_id: table_data.id,
                table_name: table_data.name.clone(),
            });
            MetaEditRequest {
                shard_info: table_data.shard_info,
                meta_edit: MetaEdit::Update(meta_update),
            }
        };
        self.space_store
            .manifest
            .apply_edit(edit_req)
            .await
            .context(WriteManifest {
                space_id: self.space.id,
                table: &table_data.name,
                table_id: table_data.id,
            })?;

        Ok(true)
    }
}
