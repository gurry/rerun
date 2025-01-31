use std::{collections::BTreeMap, time::Duration};

use ahash::{HashMap, HashSet};
use web_time::Instant;

use re_log_types::{
    DataCell, EntityPath, EntityPathHash, ResolvedTimeRange, RowId, TimeInt, TimePoint, Timeline,
    VecDequeRemovalExt as _,
};
use re_types_core::{ComponentName, SizeBytes as _};

use crate::{
    store::{IndexedBucketInner, IndexedTable},
    DataStore, DataStoreStats, StoreDiff, StoreDiffKind, StoreEvent,
};

// ---

#[derive(Debug, Clone, Copy)]
pub enum GarbageCollectionTarget {
    /// Try to drop _at least_ the given fraction.
    ///
    /// The fraction must be a float in the range [0.0 : 1.0].
    DropAtLeastFraction(f64),

    /// GC Everything that isn't protected
    Everything,
}

#[derive(Debug, Clone)]
pub struct GarbageCollectionOptions {
    /// What target threshold should the GC try to meet.
    pub target: GarbageCollectionTarget,

    /// How long the garbage collection in allowed to run for.
    ///
    /// Trades off latency for throughput:
    /// - A smaller `time_budget` will clear less data in a shorter amount of time, allowing for a
    ///   more responsive UI at the cost of more GC overhead and more frequent runs.
    /// - A larger `time_budget` will clear more data in a longer amount of time, increasing the
    ///   chance of UI freeze frames but decreasing GC overhead and running less often.
    ///
    /// The default is an unbounded time budget (i.e. throughput only).
    pub time_budget: Duration,

    /// How many component revisions to preserve on each timeline.
    pub protect_latest: usize,

    /// Whether to purge tables that no longer contain any data
    pub purge_empty_tables: bool,

    /// Components which should not be protected from GC when using `protect_latest`
    pub dont_protect: HashSet<ComponentName>,

    /// Whether to enable batched bucket drops.
    ///
    /// Disabled by default as it is currently slower in most cases (somehow).
    pub enable_batching: bool,
}

impl GarbageCollectionOptions {
    pub fn gc_everything() -> Self {
        GarbageCollectionOptions {
            target: GarbageCollectionTarget::Everything,
            time_budget: std::time::Duration::MAX,
            protect_latest: 0,
            purge_empty_tables: true,
            dont_protect: Default::default(),
            enable_batching: false,
        }
    }
}

impl std::fmt::Display for GarbageCollectionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GarbageCollectionTarget::DropAtLeastFraction(p) => {
                write!(f, "DropAtLeast({:.3}%)", *p * 100.0)
            }
            GarbageCollectionTarget::Everything => write!(f, "Everything"),
        }
    }
}

impl DataStore {
    /// Triggers a garbage collection according to the desired `target`.
    ///
    /// Garbage collection's performance is bounded by the number of buckets in each table (for
    /// each `RowId`, we have to find the corresponding bucket, which is roughly `O(log(n))`) as
    /// well as the number of rows in each of those buckets (for each `RowId`, we have to sort the
    /// corresponding bucket (roughly `O(n*log(n))`) and then find the corresponding row (roughly
    /// `O(log(n))`.
    /// The size of the data itself has no impact on performance.
    ///
    /// Returns the list of `RowId`s that were purged from the store.
    ///
    /// ## Semantics
    ///
    /// Garbage collection works on a row-level basis and is driven by [`RowId`] order,
    /// i.e. the order defined by the clients' wall-clocks, allowing it to drop data across
    /// the different timelines in a fair, deterministic manner.
    /// Similarly, out-of-order data is supported out of the box.
    ///
    /// The garbage collector doesn't deallocate data in and of itself: all it does is drop the
    /// store's internal references to that data (the `DataCell`s), which will be deallocated once
    /// their reference count reaches 0.
    ///
    /// ## Limitations
    ///
    /// The garbage collector has limited support for latest-at semantics. The configuration option:
    /// [`GarbageCollectionOptions::protect_latest`] will protect the N latest values of each
    /// component on each timeline. The only practical guarantee this gives is that a latest-at query
    /// with a value of max-int will be unchanged. However, latest-at queries from other arbitrary
    /// points in time may provide different results pre- and post- GC.
    pub fn gc(&mut self, options: &GarbageCollectionOptions) -> (Vec<StoreEvent>, DataStoreStats) {
        re_tracing::profile_function!();

        self.gc_id += 1;

        let stats_before = DataStoreStats::from_store(self);

        let (initial_num_rows, initial_num_bytes) = stats_before.total_rows_and_bytes();

        let protected_rows =
            self.find_all_protected_rows(options.protect_latest, &options.dont_protect);

        let mut diffs = match options.target {
            GarbageCollectionTarget::DropAtLeastFraction(p) => {
                assert!((0.0..=1.0).contains(&p));

                let num_bytes_to_drop = initial_num_bytes * p;
                let target_num_bytes = initial_num_bytes - num_bytes_to_drop;

                re_log::trace!(
                    kind = "gc",
                    id = self.gc_id,
                    %options.target,
                    initial_num_rows = re_format::format_uint(initial_num_rows),
                    initial_num_bytes = re_format::format_bytes(initial_num_bytes),
                    target_num_bytes = re_format::format_bytes(target_num_bytes),
                    drop_at_least_num_bytes = re_format::format_bytes(num_bytes_to_drop),
                    "starting GC"
                );

                self.gc_drop_at_least_num_bytes(options, num_bytes_to_drop, &protected_rows)
            }
            GarbageCollectionTarget::Everything => {
                re_log::trace!(
                    kind = "gc",
                    id = self.gc_id,
                    %options.target,
                    initial_num_rows = re_format::format_uint(initial_num_rows),
                    initial_num_bytes = re_format::format_bytes(initial_num_bytes),
                    "starting GC"
                );

                self.gc_drop_at_least_num_bytes(options, f64::INFINITY, &protected_rows)
            }
        };

        if options.purge_empty_tables {
            diffs.extend(self.purge_empty_tables());
        }

        #[cfg(debug_assertions)]
        self.sanity_check().unwrap();

        // NOTE: only temporal data and row metadata get purged!
        let stats_after = DataStoreStats::from_store(self);
        let (new_num_rows, new_num_bytes) = stats_after.total_rows_and_bytes();

        re_log::trace!(
            kind = "gc",
            id = self.gc_id,
            %options.target,
            initial_num_rows = re_format::format_uint(initial_num_rows),
            initial_num_bytes = re_format::format_bytes(initial_num_bytes),
            new_num_rows = re_format::format_uint(new_num_rows),
            new_num_bytes = re_format::format_bytes(new_num_bytes),
            "GC done"
        );

        let stats_diff = stats_before - stats_after;

        let events: Vec<_> = diffs
            .into_iter()
            .map(|diff| StoreEvent {
                store_id: self.id.clone(),
                store_generation: self.generation(),
                event_id: self
                    .event_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                diff,
            })
            .collect();

        {
            if cfg!(debug_assertions) {
                let any_event_other_than_deletion =
                    events.iter().any(|e| e.kind != StoreDiffKind::Deletion);
                assert!(!any_event_other_than_deletion);
            }

            Self::on_events(&events);
        }

        (events, stats_diff)
    }

    /// Tries to drop _at least_ `num_bytes_to_drop` bytes of data from the store.
    fn gc_drop_at_least_num_bytes(
        &mut self,
        options: &GarbageCollectionOptions,
        mut num_bytes_to_drop: f64,
        protected_rows: &HashSet<RowId>,
    ) -> Vec<StoreDiff> {
        re_tracing::profile_function!();

        let mut diffs = Vec::new();

        // The algorithm is straightforward:
        // 1. Accumulate a bunch of `RowId`s in ascending order, starting from the beginning of time.
        // 2. Check if any `RowId` in the batch is protected, in which case the entire batch is
        //    considered protected and cannot be dropped all at once.
        // 3. Send the batch to `drop_batch` to handle the actual deletion.
        // 4. Removed the dropped rows from the metadata registry.

        let batch_size = (self.config.indexed_bucket_num_rows as usize).saturating_mul(2);
        let batch_size = batch_size.clamp(64, 4096);

        let mut batch: Vec<(TimePoint, (EntityPathHash, RowId))> = Vec::with_capacity(batch_size);
        let mut batch_is_protected = false;

        let Self {
            metadata_registry,
            tables,
            ..
        } = self;

        let now = Instant::now();
        for (&row_id, (timepoint, entity_path_hash)) in &metadata_registry.registry {
            if protected_rows.contains(&row_id) {
                batch_is_protected = true;
                continue;
            }

            batch.push((timepoint.clone(), (*entity_path_hash, row_id)));
            if batch.len() < batch_size {
                continue;
            }

            let dropped = Self::drop_batch(
                options,
                tables,
                &mut num_bytes_to_drop,
                &batch,
                batch_is_protected,
            );

            // Only decrement the metadata size trackers if we're actually certain that we'll drop
            // that RowId in the end.
            for dropped in dropped {
                let metadata_dropped_size_bytes = dropped.row_id.total_size_bytes()
                    + dropped.timepoint().total_size_bytes()
                    + dropped.entity_path.hash().total_size_bytes();
                metadata_registry.heap_size_bytes = metadata_registry
                    .heap_size_bytes
                    .checked_sub(metadata_dropped_size_bytes)
                    .unwrap_or_else(|| {
                        re_log::debug!(
                            entity_path = %dropped.entity_path,
                            current = metadata_registry.heap_size_bytes,
                            removed = metadata_dropped_size_bytes,
                            "book keeping underflowed"
                        );
                        u64::MIN
                    });
                num_bytes_to_drop -= metadata_dropped_size_bytes as f64;

                diffs.push(dropped);
            }

            if now.elapsed() >= options.time_budget || num_bytes_to_drop <= 0.0 {
                break;
            }

            batch.clear();
            batch_is_protected = false;
        }

        // Handle leftovers.
        {
            let dropped = Self::drop_batch(
                options,
                tables,
                &mut num_bytes_to_drop,
                &batch,
                batch_is_protected,
            );

            // Only decrement the metadata size trackers if we're actually certain that we'll drop
            // that RowId in the end.
            for dropped in dropped {
                let metadata_dropped_size_bytes = dropped.row_id.total_size_bytes()
                    + dropped.timepoint().total_size_bytes()
                    + dropped.entity_path.hash().total_size_bytes();
                metadata_registry.heap_size_bytes = metadata_registry
                    .heap_size_bytes
                    .checked_sub(metadata_dropped_size_bytes)
                    .unwrap_or_else(|| {
                        re_log::debug!(
                            entity_path = %dropped.entity_path,
                            current = metadata_registry.heap_size_bytes,
                            removed = metadata_dropped_size_bytes,
                            "book keeping underflowed"
                        );
                        u64::MIN
                    });
                num_bytes_to_drop -= metadata_dropped_size_bytes as f64;

                diffs.push(dropped);
            }
        }

        // Purge the removed rows from the metadata_registry.
        // This is safe because the entire GC process is driven by RowId-order.
        for diff in &diffs {
            metadata_registry.remove(&diff.row_id);
        }

        diffs
    }

    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn drop_batch(
        options: &GarbageCollectionOptions,
        tables: &mut BTreeMap<(EntityPathHash, Timeline), IndexedTable>,
        num_bytes_to_drop: &mut f64,
        batch: &[(TimePoint, (EntityPathHash, RowId))],
        batch_is_protected: bool,
    ) -> Vec<StoreDiff> {
        let &GarbageCollectionOptions {
            enable_batching, ..
        } = options;

        let mut diffs = Vec::new();

        // The algorithm is straightforward:
        // 1. If the batch isn't protected, find and drop all buckets that are guaranteed to
        //    contain only rows older than the ones in the batch.
        // 2. Check how many bytes were dropped; continue if we haven't met our objective.
        // 3. Fallback to deletion of individual rows.
        // 4. Check how many bytes were dropped; continue if we haven't met our objective.

        // NOTE: The batch is already sorted by definition since it's extracted from the registry's btreemap.
        let max_row_id = batch.last().map(|(_, (_, row_id))| *row_id);

        if enable_batching && max_row_id.is_some() && !batch_is_protected {
            // NOTE: unwrap cannot fail but just a precaution in case this code moves around…
            let max_row_id = max_row_id.unwrap_or(RowId::ZERO);

            let mut batch_removed: HashMap<RowId, StoreDiff> = HashMap::default();
            let mut cur_entity_path_hash = None;

            // NOTE: We _must_  go through all tables no matter what, since the batch might contain
            // any number of distinct entities.
            for ((entity_path_hash, _), table) in &mut *tables {
                let (removed, num_bytes_removed) = table.try_drop_bucket(max_row_id);

                *num_bytes_to_drop -= num_bytes_removed as f64;

                if cur_entity_path_hash != Some(*entity_path_hash) {
                    diffs.extend(batch_removed.drain().map(|(_, diff)| diff));

                    cur_entity_path_hash = Some(*entity_path_hash);
                }

                for mut removed in removed {
                    batch_removed
                        .entry(removed.row_id)
                        .and_modify(|diff| {
                            diff.times.extend(std::mem::take(&mut removed.times));
                        })
                        .or_insert(removed);
                }
            }

            diffs.extend(batch_removed.drain().map(|(_, diff)| diff));
        }

        if *num_bytes_to_drop <= 0.0 {
            return diffs;
        }

        for (timepoint, (entity_path_hash, row_id)) in batch {
            let mut diff: Option<StoreDiff> = None;

            // find all tables that could possibly contain this `RowId`
            for (&timeline, &time) in timepoint {
                if let Some(table) = tables.get_mut(&(*entity_path_hash, timeline)) {
                    let (removed, num_bytes_removed) = table.try_drop_row(*row_id, time);
                    if let Some(inner) = diff.as_mut() {
                        if let Some(removed) = removed {
                            inner.times.extend(removed.times);
                        }
                    } else {
                        diff = removed;
                    }
                    *num_bytes_to_drop -= num_bytes_removed as f64;
                }
            }

            diffs.extend(diff);

            if *num_bytes_to_drop <= 0.0 {
                break;
            }
        }

        diffs
    }

    /// For each `EntityPath`, `Timeline`, `Component` find the N latest [`RowId`]s.
    //
    // TODO(jleibs): More complex functionality might required expanding this to also
    // *ignore* specific entities, components, timelines, etc. for this protection.
    //
    // TODO(jleibs): `RowId`s should never overlap between entities. Creating a single large
    // HashSet might actually be sub-optimal here. Consider switching to a map of
    // `EntityPath` -> `HashSet<RowId>`.
    // Update: this is true-er than ever before now that RowIds are truly unique!
    fn find_all_protected_rows(
        &mut self,
        target_count: usize,
        dont_protect: &HashSet<ComponentName>,
    ) -> HashSet<RowId> {
        re_tracing::profile_function!();

        if target_count == 0 {
            return Default::default();
        }

        // We need to sort to be able to determine latest-at.
        self.sort_indices_if_needed();

        let mut protected_rows: HashSet<RowId> = Default::default();

        // Find all protected rows in regular indexed tables
        for table in self.tables.values() {
            let mut components_to_find: HashMap<ComponentName, usize> = table
                .all_components
                .iter()
                .filter(|c| !dont_protect.contains(*c))
                .map(|c| (*c, target_count))
                .collect();

            for bucket in table.buckets.values().rev() {
                for (component, count) in &mut components_to_find {
                    if *count == 0 {
                        continue;
                    }
                    let inner = bucket.inner.read();
                    // TODO(jleibs): If the entire column for a component is empty, we should
                    // make sure the column is dropped so we don't have to iterate over a
                    // bunch of Nones.
                    if let Some(column) = inner.columns.get(component) {
                        for row in column
                            .iter()
                            .enumerate()
                            .rev()
                            .filter_map(|(row_index, cell)| {
                                cell.as_ref().and_then(|_| inner.col_row_id.get(row_index))
                            })
                            .take(*count)
                        {
                            *count -= 1;
                            protected_rows.insert(*row);
                        }
                    }
                }
            }
        }

        protected_rows
    }

    /// Remove any tables which contain only components which are empty.
    // TODO(jleibs): We could optimize this further by also erasing empty columns.
    fn purge_empty_tables(&mut self) -> impl Iterator<Item = StoreDiff> {
        re_tracing::profile_function!();

        let mut diffs: BTreeMap<RowId, StoreDiff> = BTreeMap::default();

        self.tables.retain(|_, table| {
            // If any bucket has a non-empty component in any column, we keep it…
            for bucket in table.buckets.values() {
                let inner = bucket.inner.read();
                for column in inner.columns.values() {
                    if column
                        .iter()
                        .any(|cell| cell.as_ref().map_or(false, |cell| cell.num_instances() > 0))
                    {
                        return true;
                    }
                }
            }

            // …otherwise we can drop it.

            let entity_path = table.entity_path.clone();

            for bucket in table.buckets.values() {
                let mut inner = bucket.inner.write();

                for i in 0..inner.col_row_id.len() {
                    let row_id = inner.col_row_id[i];
                    let time = inner.col_time[i];

                    let diff = diffs
                        .entry(row_id)
                        .or_insert_with(|| StoreDiff::deletion(row_id, entity_path.clone()));

                    diff.times
                        .push((bucket.timeline, TimeInt::new_temporal(time)));

                    for column in &mut inner.columns.values_mut() {
                        let cell = column[i].take();
                        if let Some(cell) = cell {
                            diff.insert(cell);
                        }
                    }
                }
            }

            false
        });

        diffs.into_values()
    }
}

impl IndexedTable {
    /// Try to drop an entire bucket at once if it doesn't contain any `RowId` greater than `max_row_id`.
    fn try_drop_bucket(&mut self, max_row_id: RowId) -> (Vec<StoreDiff>, u64) {
        re_tracing::profile_function!();

        let entity_path = self.entity_path.clone();
        let timeline = self.timeline;

        let mut diffs: Vec<StoreDiff> = Vec::new();
        let mut dropped_num_bytes = 0u64;
        let mut dropped_num_rows = 0u64;

        let mut dropped_bucket_times = HashSet::default();

        // TODO(cmc): scaling linearly with the number of buckets could be improved, although this
        // is quite fast in practice because of the early check.
        for (bucket_time, bucket) in &self.buckets {
            let inner = &mut *bucket.inner.write();

            if inner.col_time.is_empty() || max_row_id < inner.max_row_id {
                continue;
            }

            let IndexedBucketInner {
                mut col_time,
                mut col_row_id,
                mut columns,
                size_bytes,
                ..
            } = std::mem::take(inner);

            dropped_bucket_times.insert(*bucket_time);

            while let Some(row_id) = col_row_id.pop_front() {
                let mut diff = StoreDiff::deletion(row_id, entity_path.clone());

                if let Some(time) = col_time.pop_front() {
                    diff.times.push((timeline, TimeInt::new_temporal(time)));
                }

                for (component_name, column) in &mut columns {
                    if let Some(cell) = column.pop_front().flatten() {
                        diff.cells.insert(*component_name, cell);
                    }
                }

                diffs.push(diff);
            }

            dropped_num_bytes += size_bytes;
            dropped_num_rows += col_time.len() as u64;
        }

        self.buckets
            .retain(|bucket_time, _| !dropped_bucket_times.contains(bucket_time));

        self.uphold_indexing_invariants();

        self.buckets_num_rows -= dropped_num_rows;
        self.buckets_size_bytes -= dropped_num_bytes;

        (diffs, dropped_num_bytes)
    }

    /// Tries to drop the given `row_id` from the table, which is expected to be found at the
    /// specified `time`.
    ///
    /// Returns how many bytes were actually dropped, or zero if the row wasn't found.
    fn try_drop_row(&mut self, row_id: RowId, time: TimeInt) -> (Option<StoreDiff>, u64) {
        re_tracing::profile_function!();

        let entity_path = self.entity_path.clone();
        let timeline = self.timeline;

        let table_has_more_than_one_bucket = self.buckets.len() > 1;

        let (bucket_key, bucket) = self.find_bucket_mut(time);
        let bucket_num_bytes = bucket.total_size_bytes();

        let (diff, mut dropped_num_bytes) = {
            let inner = &mut *bucket.inner.write();
            inner.try_drop_row(row_id, timeline, &entity_path, time)
        };

        // NOTE: We always need to keep at least one bucket alive, otherwise we have
        // nowhere to write to.
        if table_has_more_than_one_bucket && bucket.num_rows() == 0 {
            // NOTE: We're dropping the bucket itself in this case, rather than just its
            // contents.
            debug_assert!(
                dropped_num_bytes <= bucket_num_bytes,
                "Bucket contained more bytes than it thought"
            );
            dropped_num_bytes = bucket_num_bytes;
            self.buckets.remove(&bucket_key);

            self.uphold_indexing_invariants();
        }

        self.buckets_size_bytes -= dropped_num_bytes;
        self.buckets_num_rows -= (dropped_num_bytes > 0) as u64;

        (diff, dropped_num_bytes)
    }
}

impl IndexedBucketInner {
    /// Tries to drop the given `row_id` from the table, which is expected to be found at the
    /// specified `time`.
    ///
    /// Returns how many bytes were actually dropped, or zero if the row wasn't found.
    fn try_drop_row(
        &mut self,
        row_id: RowId,
        timeline: Timeline,
        entity_path: &EntityPath,
        time: TimeInt,
    ) -> (Option<StoreDiff>, u64) {
        self.sort();

        let IndexedBucketInner {
            is_sorted,
            time_range,
            col_time,
            col_insert_id,
            col_row_id,
            max_row_id,
            columns,
            size_bytes,
        } = self;

        let mut diff: Option<StoreDiff> = None;
        let mut dropped_num_bytes = 0u64;

        let mut row_index = col_time.partition_point(|&time2| time2 < time.as_i64());
        while col_time.get(row_index) == Some(&time.as_i64()) {
            if col_row_id[row_index] != row_id {
                row_index += 1;
                continue;
            }

            // Update the time_range min/max:
            if col_time.len() == 1 {
                // We removed the last row
                *time_range = ResolvedTimeRange::EMPTY;
            } else {
                *is_sorted = row_index == 0 || row_index.saturating_add(1) == col_row_id.len();

                // We have at least two rows, so we can safely [index] here:
                if row_index == 0 {
                    // We removed the first row, so the second row holds the new min
                    time_range.set_min(col_time[1]);
                }
                if row_index + 1 == col_time.len() {
                    // We removed the last row, so the penultimate row holds the new max
                    time_range.set_max(col_time[row_index - 1]);
                }
            }

            // col_row_id
            let Some(removed_row_id) = col_row_id.swap_remove(row_index) else {
                continue;
            };
            debug_assert_eq!(row_id, removed_row_id);
            dropped_num_bytes += removed_row_id.total_size_bytes();

            // col_time
            if let Some(row_time) = col_time.swap_remove(row_index) {
                dropped_num_bytes += row_time.total_size_bytes();
            }

            // col_insert_id (if present)
            if !col_insert_id.is_empty() {
                if let Some(insert_id) = col_insert_id.swap_remove(row_index) {
                    dropped_num_bytes += insert_id.total_size_bytes();
                }
            }

            // each data column
            for column in columns.values_mut() {
                let cell = column.0.swap_remove(row_index).flatten();

                // TODO(#1809): once datatype deduplication is in, we should really not count
                // autogenerated keys as part of the memory stats (same on write path).
                dropped_num_bytes += cell.total_size_bytes();

                if let Some(cell) = cell {
                    if let Some(inner) = diff.as_mut() {
                        inner.insert(cell);
                    } else {
                        let mut d = StoreDiff::deletion(removed_row_id, entity_path.clone());
                        d.at_timestamp(timeline, time);
                        d.insert(cell);
                        diff = Some(d);
                    }
                }
            }

            if *max_row_id == removed_row_id {
                // NOTE: We _have_ to fullscan here: the bucket is sorted by `(Time, RowId)`, there
                // could very well be a greater lurking in a lesser entry.
                *max_row_id = col_row_id.iter().max().copied().unwrap_or(RowId::ZERO);
            }

            // NOTE: A single `RowId` cannot possibly have more than one datapoint for
            // a single timeline.
            break;
        }

        *size_bytes -= dropped_num_bytes;

        (diff, dropped_num_bytes)
    }
}

// ---

impl StoreDiff {
    fn insert(&mut self, cell: DataCell) {
        self.cells.insert(cell.component_name(), cell);
    }
}
