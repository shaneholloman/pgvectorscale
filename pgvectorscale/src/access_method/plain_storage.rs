use super::{
    distance::DistanceFn,
    graph::{ListSearchNeighbor, ListSearchResult},
    graph_neighbor_store::GraphNeighborStore,
    labels::{LabelSet, LabeledVector},
    neighbor_with_distance::DistanceWithTieBreak,
    pg_vector::PgVector,
    plain_node::{ArchivedPlainNode, PlainNode, ReadablePlainNode},
    stats::{
        GreedySearchStats, StatsDistanceComparison, StatsHeapNodeRead, StatsNodeModify,
        StatsNodeRead, StatsNodeWrite, WriteStats,
    },
    storage::{ArchivedData, NodeDistanceMeasure, Storage},
    storage_common::get_index_vector_attribute,
};

use pgrx::{pg_sys::AttrNumber, PgBox, PgRelation};

use super::{meta_page::MetaPage, neighbor_with_distance::NeighborWithDistance};
use crate::access_method::node::{ReadableNode, WriteableNode};
use crate::util::{
    page::PageType, table_slot::TableSlot, tape::Tape, HeapPointer, IndexPointer, ItemPointer,
};

pub struct PlainStorage<'a> {
    pub index: &'a PgRelation,
    pub distance_fn: DistanceFn,
    heap_rel: &'a PgRelation,
    heap_attr: AttrNumber,
}

impl<'a> PlainStorage<'a> {
    pub fn new_for_build(
        index: &'a PgRelation,
        heap_rel: &'a PgRelation,
        distance_fn: DistanceFn,
    ) -> PlainStorage<'a> {
        Self {
            index,
            distance_fn,
            heap_rel,
            heap_attr: get_index_vector_attribute(index),
        }
    }

    pub fn load_for_insert(
        index_relation: &'a PgRelation,
        heap_rel: &'a PgRelation,
        distance_fn: DistanceFn,
    ) -> PlainStorage<'a> {
        Self {
            index: index_relation,
            distance_fn,
            heap_rel,
            heap_attr: get_index_vector_attribute(index_relation),
        }
    }

    pub fn load_for_search(
        index_relation: &'a PgRelation,
        heap_rel: &'a PgRelation,
        distance_fn: DistanceFn,
    ) -> PlainStorage<'a> {
        Self {
            index: index_relation,
            distance_fn,
            heap_rel,
            heap_attr: get_index_vector_attribute(index_relation),
        }
    }
}

pub enum PlainDistanceMeasure {
    Full(LabeledVector),
}

impl PlainDistanceMeasure {
    pub fn calculate_distance<S: StatsDistanceComparison>(
        distance_fn: DistanceFn,
        query: &[f32],
        vector: &[f32],
        stats: &mut S,
    ) -> f32 {
        assert!(!vector.is_empty());
        assert!(vector.len() == query.len());
        stats.record_full_distance_comparison();
        (distance_fn)(query, vector)
    }
}

/* This is only applicable to plain, so keep here not in storage_common */
pub struct IndexFullDistanceMeasure<'a> {
    readable_node: ReadablePlainNode<'a>,
    storage: &'a PlainStorage<'a>,
}

impl<'a> IndexFullDistanceMeasure<'a> {
    pub unsafe fn with_index_pointer<T: StatsNodeRead>(
        storage: &'a PlainStorage<'a>,
        index_pointer: IndexPointer,
        stats: &mut T,
    ) -> Self {
        let rn = unsafe { PlainNode::read(storage.index, index_pointer, stats) };
        Self {
            readable_node: rn,
            storage,
        }
    }

    pub unsafe fn with_readable_node(
        storage: &'a PlainStorage<'a>,
        readable_node: ReadablePlainNode<'a>,
    ) -> Self {
        Self {
            readable_node,
            storage,
        }
    }
}

impl NodeDistanceMeasure for IndexFullDistanceMeasure<'_> {
    unsafe fn get_distance<T: StatsNodeRead + StatsDistanceComparison>(
        &self,
        index_pointer: IndexPointer,
        stats: &mut T,
    ) -> f32 {
        let rn1 = PlainNode::read(self.storage.index, index_pointer, stats);
        let rn2 = &self.readable_node;
        let node1 = rn1.get_archived_node();
        let node2 = rn2.get_archived_node();
        assert!(!node1.vector.is_empty());
        assert!(node1.vector.len() == node2.vector.len());
        let vec1 = node1.vector.as_slice();
        let vec2 = node2.vector.as_slice();
        (self.storage.get_distance_function())(vec1, vec2)
    }
}

//todo move to storage_common
pub struct PlainStorageLsnPrivateData {
    pub heap_pointer: HeapPointer,
    pub neighbors: Vec<ItemPointer>,
}

impl PlainStorageLsnPrivateData {
    pub fn new(
        index_pointer_to_node: IndexPointer,
        node: &ArchivedPlainNode,
        gns: &GraphNeighborStore,
    ) -> Self {
        let heap_pointer = node.heap_item_pointer.deserialize_item_pointer();
        let neighbors = match gns {
            GraphNeighborStore::Disk => node.get_index_pointer_to_neighbors(),
            GraphNeighborStore::Builder(b) => b.get_neighbors(index_pointer_to_node),
        };
        Self {
            heap_pointer,
            neighbors,
        }
    }
}

impl Storage for PlainStorage<'_> {
    type QueryDistanceMeasure = PlainDistanceMeasure;
    type NodeDistanceMeasure<'b>
        = IndexFullDistanceMeasure<'b>
    where
        Self: 'b;
    type ArchivedType<'b>
        = ArchivedPlainNode
    where
        Self: 'b;
    type LSNPrivateData = PlainStorageLsnPrivateData;

    fn page_type() -> PageType {
        PageType::Node
    }

    fn create_node<S: StatsNodeWrite>(
        &self,
        full_vector: &[f32],
        _labels: Option<LabelSet>,
        heap_pointer: HeapPointer,
        meta_page: &MetaPage,
        tape: &mut Tape,
        stats: &mut S,
    ) -> ItemPointer {
        //OPT: avoid the clone?
        let node = PlainNode::new_for_full_vector(full_vector.to_vec(), heap_pointer, meta_page);
        let index_pointer: IndexPointer = node.write(tape, stats);
        index_pointer
    }

    fn start_training(&mut self, _meta_page: &super::meta_page::MetaPage) {}

    fn add_sample(&mut self, _sample: &[f32]) {}

    fn finish_training(&mut self, _meta_page: &mut MetaPage, _stats: &mut WriteStats) {}

    fn finalize_node_at_end_of_build<S: StatsNodeRead + StatsNodeModify>(
        &mut self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &[NeighborWithDistance],
        stats: &mut S,
    ) {
        let mut node = unsafe { PlainNode::modify(self.index, index_pointer, stats) };
        let mut archived = node.get_archived_node();
        archived.as_mut().set_neighbors(neighbors, meta);
        node.commit();
    }

    unsafe fn get_node_distance_measure<'b, S: StatsNodeRead>(
        &'b self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> Self::NodeDistanceMeasure<'b> {
        IndexFullDistanceMeasure::with_index_pointer(self, index_pointer, stats)
    }

    fn get_query_distance_measure(&self, query: LabeledVector) -> PlainDistanceMeasure {
        PlainDistanceMeasure::Full(query)
    }

    fn get_full_distance_for_resort<S: StatsHeapNodeRead + StatsDistanceComparison>(
        &self,
        scan: &PgBox<pgrx::pg_sys::IndexScanDescData>,
        qdm: &Self::QueryDistanceMeasure,
        _index_pointer: IndexPointer,
        heap_pointer: HeapPointer,
        meta_page: &MetaPage,
        stats: &mut S,
    ) -> Option<f32> {
        /* Plain storage only needs to resort when the index is using less dimensions than the underlying data. */
        assert!(meta_page.get_num_dimensions() > meta_page.get_num_dimensions_to_index());

        let slot_opt = unsafe {
            TableSlot::from_index_heap_pointer(self.heap_rel, heap_pointer, scan.xs_snapshot, stats)
        };
        let slot = slot_opt?;
        match qdm {
            PlainDistanceMeasure::Full(query) => {
                let datum = unsafe {
                    slot.get_attribute(self.heap_attr)
                        .expect("vector attribute should exist in the heap")
                };
                let vec = unsafe { PgVector::from_datum(datum, meta_page, false, true) };
                Some(self.get_distance_function()(
                    vec.to_full_slice(),
                    query.vec().to_full_slice(),
                ))
            }
        }
    }
    fn get_neighbors_with_distances_from_disk<S: StatsNodeRead + StatsDistanceComparison>(
        &self,
        neighbors_of: ItemPointer,
        result: &mut Vec<NeighborWithDistance>,
        stats: &mut S,
    ) {
        let rn = unsafe { PlainNode::read(self.index, neighbors_of, stats) };
        // Copy neighbors before giving ownership of `rn`` to the distance state
        let neighbors: Vec<_> = rn.get_archived_node().iter_neighbors().collect();
        let dist_state = unsafe { IndexFullDistanceMeasure::with_readable_node(self, rn) };
        for n in neighbors.into_iter() {
            // TODO: we are reading node twice
            let dist = unsafe { dist_state.get_distance(n, stats) };
            result.push(NeighborWithDistance::new(
                n,
                DistanceWithTieBreak::new(dist, neighbors_of, n),
                None,
            ))
        }
    }

    /* get_lsn and visit_lsn are different because the distance
    comparisons for SBQ get the vector from different places */
    fn create_lsn_for_start_node(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        index_pointer: ItemPointer,
        gns: &GraphNeighborStore,
    ) -> Option<ListSearchNeighbor<Self::LSNPrivateData>> {
        if !lsr.prepare_insert(index_pointer) {
            // Node already processed, skip it
            return None;
        }

        let rn = unsafe { PlainNode::read(self.index, index_pointer, &mut lsr.stats) };
        let node = rn.get_archived_node();

        let distance = match lsr.sdm.as_ref().unwrap() {
            PlainDistanceMeasure::Full(query) => PlainDistanceMeasure::calculate_distance(
                self.distance_fn,
                query.vec().to_index_slice(),
                node.vector.as_slice(),
                &mut lsr.stats,
            ),
        };

        Some(ListSearchNeighbor::new(
            index_pointer,
            lsr.create_distance_with_tie_break(distance, index_pointer),
            PlainStorageLsnPrivateData::new(index_pointer, node, gns),
            None,
        ))
    }

    fn visit_lsn(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        lsn_idx: usize,
        gns: &GraphNeighborStore,
        no_filter: bool,
    ) {
        assert!(no_filter, "Plain storage does not support label filters");

        let lsn = lsr.get_lsn_by_idx(lsn_idx);
        //clone needed so we don't continue to borrow lsr
        let neighbors = lsn.get_private_data().neighbors.clone();

        for &neighbor_index_pointer in neighbors.iter() {
            if !lsr.prepare_insert(neighbor_index_pointer) {
                continue;
            }

            let rn_neighbor =
                unsafe { PlainNode::read(self.index, neighbor_index_pointer, &mut lsr.stats) };
            let node_neighbor = rn_neighbor.get_archived_node();

            let distance = match lsr.sdm.as_ref().unwrap() {
                PlainDistanceMeasure::Full(query) => PlainDistanceMeasure::calculate_distance(
                    self.distance_fn,
                    query.vec().to_index_slice(),
                    node_neighbor.vector.as_slice(),
                    &mut lsr.stats,
                ),
            };
            let lsn = ListSearchNeighbor::new(
                neighbor_index_pointer,
                lsr.create_distance_with_tie_break(distance, neighbor_index_pointer),
                PlainStorageLsnPrivateData::new(neighbor_index_pointer, node_neighbor, gns),
                None,
            );

            lsr.insert_neighbor(lsn);
        }
    }

    fn return_lsn(
        &self,
        lsn: &ListSearchNeighbor<Self::LSNPrivateData>,
        _stats: &mut GreedySearchStats,
    ) -> HeapPointer {
        lsn.get_private_data().heap_pointer
    }

    fn set_neighbors_on_disk<S: StatsNodeModify + StatsNodeRead>(
        &self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &[NeighborWithDistance],
        stats: &mut S,
    ) {
        let mut node = unsafe { PlainNode::modify(self.index, index_pointer, stats) };
        let mut archived = node.get_archived_node();
        archived.as_mut().set_neighbors(neighbors, meta);
        node.commit();
    }

    fn get_distance_function(&self) -> DistanceFn {
        self.distance_fn
    }

    fn get_labels<S: StatsNodeRead>(
        &self,
        _index_pointer: IndexPointer,
        _stats: &mut S,
    ) -> Option<LabelSet> {
        None
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {

    use pgrx::*;

    use crate::access_method::distance::DistanceType;

    #[pg_test]
    unsafe fn test_plain_storage_index_creation_many_neighbors() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            DistanceType::Cosine,
            "num_neighbors=38, storage_layout = plain",
            "plain_many_neighbors",
            1536,
        )?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_plain_storage_index_creation_few_neighbors() -> spi::Result<()> {
        //a test with few neighbors tests the case that nodes share a page, which has caused deadlocks in the past.
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            DistanceType::Cosine,
            "num_neighbors=10, storage_layout = plain",
            "plain_few_neighbors",
            1536,
        )?;
        Ok(())
    }

    #[test]
    fn test_plain_storage_delete_vacuum_plain() {
        crate::access_method::vacuum::tests::test_delete_vacuum_plain_scaffold(
            "num_neighbors = 38, storage_layout = plain",
        );
    }

    #[test]
    fn test_plain_storage_delete_vacuum_full() {
        crate::access_method::vacuum::tests::test_delete_vacuum_full_scaffold(
            "num_neighbors = 38, storage_layout = plain",
        );
    }

    #[test]
    fn test_plain_storage_update_with_null() {
        crate::access_method::vacuum::tests::test_update_with_null_scaffold(
            "num_neighbors = 38, storage_layout = plain",
        );
    }

    #[pg_test]
    unsafe fn test_plain_storage_empty_table_insert() -> spi::Result<()> {
        crate::access_method::build::tests::test_empty_table_insert_scaffold(
            "num_neighbors=38, storage_layout = plain",
        )
    }

    #[pg_test]
    unsafe fn test_plain_storage_insert_empty_insert() -> spi::Result<()> {
        crate::access_method::build::tests::test_insert_empty_insert_scaffold(
            "num_neighbors=38, storage_layout = plain",
        )
    }

    #[pg_test]
    unsafe fn test_plain_storage_num_dimensions_cosine() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            DistanceType::Cosine,
            "num_neighbors=38, storage_layout = plain, num_dimensions=768",
            "plain_num_dimensions",
            3072,
        )?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_plain_storage_num_dimensions_l2() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            DistanceType::L2,
            "num_neighbors=38, storage_layout = plain, num_dimensions=768",
            "plain_num_dimensions",
            3072,
        )?;
        Ok(())
    }

    #[pg_test]
    #[should_panic]
    unsafe fn test_plain_storage_num_dimensions_ip() -> spi::Result<()> {
        // Should panic because combination of inner product and plain storage
        // is not supported.
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            DistanceType::InnerProduct,
            "num_neighbors=38, storage_layout = plain, num_dimensions=768",
            "plain_num_dimensions",
            3072,
        )?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_plain_storage_index_updates_cosine() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_updates(
            DistanceType::Cosine,
            "storage_layout = plain, num_neighbors=30",
            50,
            "plain",
        )?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_plain_storage_index_updates_l2() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_updates(
            DistanceType::L2,
            "storage_layout = plain, num_neighbors=30",
            50,
            "plain",
        )?;
        Ok(())
    }

    #[pg_test]
    #[should_panic]
    unsafe fn test_plain_storage_index_updates_ip() -> spi::Result<()> {
        // Should panic because combination of inner product and plain storage
        // is not supported.
        crate::access_method::build::tests::test_index_updates(
            DistanceType::InnerProduct,
            "storage_layout = plain, num_neighbors=30",
            50,
            "plain",
        )?;
        Ok(())
    }
}
