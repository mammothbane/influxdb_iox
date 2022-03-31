use std::{collections::HashMap, sync::Arc};

use backoff::{Backoff, BackoffConfig};
use data_types2::TableId;
use query::{provider::ChunkPruner, QueryChunk};
use schema::Schema;

use crate::{chunk::ParquetChunkAdapter, tombstone::QuerierTombstone};

use self::query_access::QuerierTableChunkPruner;

mod query_access;

#[cfg(test)]
mod test_util;

/// Table representation for the querier.
#[derive(Debug)]
pub struct QuerierTable {
    /// Backoff config for IO operations.
    backoff_config: BackoffConfig,

    /// Table name.
    name: Arc<str>,

    /// Table ID.
    id: TableId,

    /// Table schema.
    schema: Arc<Schema>,

    /// Interface to create chunks for this table.
    chunk_adapter: Arc<ParquetChunkAdapter>,
}

impl QuerierTable {
    /// Create new table.
    pub fn new(
        backoff_config: BackoffConfig,
        id: TableId,
        name: Arc<str>,
        schema: Arc<Schema>,
        chunk_adapter: Arc<ParquetChunkAdapter>,
    ) -> Self {
        Self {
            backoff_config,
            name,
            id,
            schema,
            chunk_adapter,
        }
    }

    /// Table name.
    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// Table ID.
    pub fn id(&self) -> TableId {
        self.id
    }

    /// Schema.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Query all chunks within this table.
    ///
    /// This currently contains all parquet files linked to their unprocessed tombstones.
    pub async fn chunks(&self) -> Vec<Arc<dyn QueryChunk>> {
        // get parquet files and tombstones in a single catalog transaction
        // TODO: figure out some form of caching
        let (parquet_files, tombstones) = Backoff::new(&self.backoff_config)
            .retry_all_errors::<_, _, _, iox_catalog::interface::Error>(
                "get parquet files and tombstones for table",
                || async {
                    let mut txn = self.chunk_adapter.catalog().start_transaction().await?;

                    let parquet_files = txn
                        .parquet_files()
                        .list_by_table_not_to_delete(self.id)
                        .await?;

                    let tombstones = txn.tombstones().list_by_table(self.id).await?;

                    txn.commit().await?;

                    Ok((parquet_files, tombstones))
                },
            )
            .await
            .expect("retry forever");

        // convert parquet files and tombstones to nicer objects
        let mut chunks = Vec::with_capacity(parquet_files.len());
        for parquet_file in parquet_files {
            if let Some(chunk) = self.chunk_adapter.new_querier_chunk(parquet_file).await {
                chunks.push(chunk);
            }
        }
        let querier_tombstones: Vec<_> =
            tombstones.into_iter().map(QuerierTombstone::from).collect();

        // match chunks and tombstones
        let mut tombstones_by_sequencer: HashMap<_, Vec<_>> = HashMap::new();
        for tombstone in querier_tombstones {
            tombstones_by_sequencer
                .entry(tombstone.sequencer_id())
                .or_default()
                .push(tombstone);
        }
        let mut chunks2 = Vec::with_capacity(chunks.len());
        for chunk in chunks.into_iter() {
            let chunk = if let Some(tombstones) =
                tombstones_by_sequencer.get(&chunk.meta().sequencer_id())
            {
                let mut delete_predicates = Vec::with_capacity(tombstones.len());
                for tombstone in tombstones {
                    // check conditions that don't need catalog access first to avoid unnecessary catalog load

                    // Check if tombstone even applies to the sequence number range within the parquet file. There
                    // are the following cases here:
                    //
                    // 1. Tombstone comes before chunk min sequencer number:
                    //    There is no way the tombstone can affect the chunk.
                    // 2. Tombstone comes after chunk max sequencer number:
                    //    Tombstone affects whole chunk (it might be marked as processed though, we'll check that
                    //    further down).
                    // 3. Tombstone is in the min-max sequencer number range of the chunk:
                    //    Technically the querier has NO way to determine the rows that are affected by the tombstone
                    //    since we have no row-level sequence numbers. Such a file can be created by two sources -- the
                    //    ingester and the compactor. The ingester must have materialized the tombstone while creating
                    //    the parquet file, so the querier can skip it. The compactor also materialized the tombstones,
                    //    so we can skip it as well. In the compactor case the tombstone will even be marked as
                    //    processed.
                    //
                    // So the querier only needs to consider the tombstone in case 2.
                    if tombstone.sequence_number() <= chunk.meta().max_sequence_number() {
                        continue;
                    }

                    // TODO: also consider time ranges (https://github.com/influxdata/influxdb_iox/issues/4086)

                    // check if tombstone is marked as processed
                    if self
                        .chunk_adapter
                        .catalog_cache()
                        .processed_tombstones()
                        .exists(
                            chunk
                                .parquet_file_id()
                                .expect("just created from a parquet file"),
                            tombstone.tombstone_id(),
                        )
                        .await
                    {
                        continue;
                    }

                    delete_predicates.push(Arc::clone(tombstone.delete_predicate()));
                }
                chunk.with_delete_predicates(delete_predicates)
            } else {
                chunk
            };

            chunks2.push(Arc::new(chunk) as Arc<dyn QueryChunk>);
        }

        chunks2
    }

    /// Get a chunk pruner that can be used to prune chunks retrieved via [`chunks`](Self::chunks)
    pub fn chunk_pruner(&self) -> Arc<dyn ChunkPruner> {
        Arc::new(QuerierTableChunkPruner {})
    }
}

#[cfg(test)]
mod tests {
    use data_types2::{ChunkId, ColumnType};
    use iox_tests::util::{now, TestCatalog};

    use crate::table::test_util::querier_table;

    #[tokio::test]
    async fn test_chunks() {
        let catalog = TestCatalog::new();

        let ns = catalog.create_namespace("ns").await;

        let table1 = ns.create_table("table1").await;
        let table2 = ns.create_table("table2").await;

        let sequencer1 = ns.create_sequencer(1).await;
        let sequencer2 = ns.create_sequencer(2).await;

        let partition11 = table1
            .with_sequencer(&sequencer1)
            .create_partition("k")
            .await;
        let partition12 = table1
            .with_sequencer(&sequencer2)
            .create_partition("k")
            .await;
        let partition21 = table2
            .with_sequencer(&sequencer1)
            .create_partition("k")
            .await;

        table1.create_column("foo", ColumnType::I64).await;
        table2.create_column("foo", ColumnType::I64).await;

        let querier_table = querier_table(&catalog, &table1).await;

        // no parquet files yet
        assert!(querier_table.chunks().await.is_empty());

        let file111 = partition11
            .create_parquet_file_with_min_max(
                "table1 foo=1 11",
                1,
                2,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let file112 = partition11
            .create_parquet_file_with_min_max(
                "table1 foo=2 22",
                3,
                4,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let file113 = partition11
            .create_parquet_file_with_min_max(
                "table1 foo=3 33",
                5,
                6,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let file114 = partition11
            .create_parquet_file_with_min_max(
                "table1 foo=4 44",
                7,
                8,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let file115 = partition11
            .create_parquet_file_with_min_max(
                "table1 foo=5 55",
                9,
                10,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let file121 = partition12
            .create_parquet_file_with_min_max(
                "table1 foo=5 55",
                1,
                2,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;
        let _file211 = partition21
            .create_parquet_file_with_min_max(
                "table2 foo=6 66",
                1,
                2,
                now().timestamp_nanos(),
                now().timestamp_nanos(),
            )
            .await;

        file111.flag_for_delete().await;

        let tombstone1 = table1
            .with_sequencer(&sequencer1)
            .create_tombstone(7, 1, 100, "foo=1")
            .await;
        tombstone1.mark_processed(&file112).await;
        let tombstone2 = table1
            .with_sequencer(&sequencer1)
            .create_tombstone(8, 1, 100, "foo=1")
            .await;
        tombstone2.mark_processed(&file112).await;

        // now we have some files
        // this contains all files except for:
        // - file111: marked for delete
        // - file221: wrong table
        let mut chunks = querier_table.chunks().await;
        chunks.sort_by_key(|c| c.id());
        assert_eq!(chunks.len(), 5);

        // check IDs
        assert_eq!(
            chunks[0].id(),
            ChunkId::new_test(file112.parquet_file.id.get() as u128),
        );
        assert_eq!(
            chunks[1].id(),
            ChunkId::new_test(file113.parquet_file.id.get() as u128),
        );
        assert_eq!(
            chunks[2].id(),
            ChunkId::new_test(file114.parquet_file.id.get() as u128),
        );
        assert_eq!(
            chunks[3].id(),
            ChunkId::new_test(file115.parquet_file.id.get() as u128),
        );
        assert_eq!(
            chunks[4].id(),
            ChunkId::new_test(file121.parquet_file.id.get() as u128),
        );

        // check delete predicates
        // file112: marked as processed
        assert_eq!(chunks[0].delete_predicates().len(), 0);
        // file113: has delete predicate
        assert_eq!(chunks[1].delete_predicates().len(), 2);
        // file114: predicates are directly within the chunk range => assume they are materialized
        assert_eq!(chunks[2].delete_predicates().len(), 0);
        // file115: came after in sequencer
        assert_eq!(chunks[3].delete_predicates().len(), 0);
        // file121: wrong sequencer
        assert_eq!(chunks[4].delete_predicates().len(), 0);
    }
}
