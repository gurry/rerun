use std::collections::BTreeMap;

use itertools::Itertools;
use nohash_hasher::IntMap;
use parking_lot::Mutex;

use re_data_store::{
    DataStore, DataStoreConfig, GarbageCollectionOptions, StoreEvent, StoreSubscriber,
};
use re_log_types::{
    ApplicationId, ComponentPath, DataCell, DataRow, DataTable, DataTableResult, EntityPath,
    EntityPathHash, LogMsg, ResolvedTimeRange, ResolvedTimeRangeF, RowId, SetStoreInfo, StoreId,
    StoreInfo, StoreKind, TimePoint, Timeline,
};
use re_query::PromiseResult;
use re_types_core::{Archetype, Loggable};

use crate::{ClearCascade, CompactedStoreEvents, Error, TimesPerTimeline};

// ----------------------------------------------------------------------------

/// See [`insert_row_with_retries`].
const MAX_INSERT_ROW_ATTEMPTS: usize = 1_000;

/// See [`insert_row_with_retries`].
const DEFAULT_INSERT_ROW_STEP_SIZE: u64 = 100;

/// See [`GarbageCollectionOptions::time_budget`].
const DEFAULT_GC_TIME_BUDGET: std::time::Duration = std::time::Duration::from_micros(3500); // empirical

/// Inserts a [`DataRow`] into the [`DataStore`], retrying in case of duplicated `RowId`s.
///
/// Retries a maximum of `num_attempts` times if the row couldn't be inserted because of a
/// duplicated [`RowId`], bumping the [`RowId`]'s internal counter by a random number
/// (up to `step_size`) between attempts.
///
/// Returns the actual [`DataRow`] that was successfully inserted, if any.
///
/// The default value of `num_attempts` (see [`MAX_INSERT_ROW_ATTEMPTS`]) should be (way) more than
/// enough for all valid use cases.
///
/// When using this function, please add a comment explaining the rationale.
fn insert_row_with_retries(
    store: &mut DataStore,
    mut row: DataRow,
    num_attempts: usize,
    step_size: u64,
) -> re_data_store::WriteResult<StoreEvent> {
    fn random_u64() -> u64 {
        let mut bytes = [0_u8; 8];
        getrandom::getrandom(&mut bytes).map_or(0, |_| u64::from_le_bytes(bytes))
    }

    for i in 0..num_attempts {
        match store.insert_row(&row) {
            Ok(event) => return Ok(event),
            Err(re_data_store::WriteError::ReusedRowId(_)) => {
                // TODO(#1894): currently we produce duplicate row-ids when hitting the "save" button.
                // This means we hit this code path when loading an .rrd file that was saved from the viewer.
                // In the future a row-id clash should probably either be considered an error (with a loud warning)
                // or an ignored idempotent operation (with the assumption that if the RowId is the same, so is the data).
                // In any case, we cannot log loudly here.
                // We also get here because of `ClearCascade`, but that could be solved by adding a random increment
                // in `on_clear_cascade` (see https://github.com/rerun-io/rerun/issues/4469).
                re_log::trace!(
                    "Found duplicated RowId ({}) during insert. Incrementing it by random offset (retry {}/{})…",
                    row.row_id,
                    i + 1,
                    num_attempts
                );
                row.row_id = row.row_id.incremented_by(random_u64() % step_size + 1);
            }
            Err(err) => return Err(err),
        }
    }

    Err(re_data_store::WriteError::ReusedRowId(row.row_id()))
}

// ----------------------------------------------------------------------------

/// An in-memory database built from a stream of [`LogMsg`]es.
///
/// NOTE: all mutation is to be done via public functions!
pub struct EntityDb {
    /// Set by whomever created this [`EntityDb`].
    ///
    /// Clones of an [`EntityDb`] gets a `None` source.
    pub data_source: Option<re_smart_channel::SmartChannelSource>,

    /// Comes in a special message, [`LogMsg::SetStoreInfo`].
    set_store_info: Option<SetStoreInfo>,

    /// Keeps track of the last time data was inserted into this store (viewer wall-clock).
    last_modified_at: web_time::Instant,

    /// The highest `RowId` in the store,
    /// which corresponds to the last edit time.
    /// Ignores deletions.
    latest_row_id: Option<RowId>,

    /// In many places we just store the hashes, so we need a way to translate back.
    entity_path_from_hash: IntMap<EntityPathHash, EntityPath>,

    /// The global-scope time tracker.
    ///
    /// For each timeline, keeps track of what times exist, recursively across all
    /// entities/components.
    ///
    /// Used for time control.
    times_per_timeline: TimesPerTimeline,

    /// A tree-view (split on path components) of the entities.
    tree: crate::EntityTree,

    /// Stores all components for all entities for all timelines.
    data_store: DataStore,

    /// The active promise resolver for this DB.
    resolver: re_query::PromiseResolver,

    /// Query caches for the data in [`Self::data_store`].
    query_caches: re_query::Caches,

    stats: IngestionStatistics,
}

impl EntityDb {
    pub fn new(store_id: StoreId) -> Self {
        let data_store =
            re_data_store::DataStore::new(store_id.clone(), DataStoreConfig::default());
        let query_caches = re_query::Caches::new(&data_store);
        Self {
            data_source: None,
            set_store_info: None,
            last_modified_at: web_time::Instant::now(),
            latest_row_id: None,
            entity_path_from_hash: Default::default(),
            times_per_timeline: Default::default(),
            tree: crate::EntityTree::root(),
            data_store,
            resolver: re_query::PromiseResolver::default(),
            query_caches,
            stats: IngestionStatistics::new(store_id),
        }
    }

    /// Helper function to create a recording from a [`StoreInfo`] and some [`DataRow`]s.
    ///
    /// This is useful to programmatically create recordings from within the viewer, which cannot
    /// use the `re_sdk`, which is not Wasm-compatible.
    pub fn from_info_and_rows(
        store_info: StoreInfo,
        rows: impl IntoIterator<Item = DataRow>,
    ) -> Result<Self, Error> {
        let mut entity_db = EntityDb::new(store_info.store_id.clone());

        entity_db.set_store_info(SetStoreInfo {
            row_id: RowId::new(),
            info: store_info,
        });
        for row in rows {
            entity_db.add_data_row(row)?;
        }

        Ok(entity_db)
    }

    #[inline]
    pub fn tree(&self) -> &crate::EntityTree {
        &self.tree
    }

    #[inline]
    pub fn data_store(&self) -> &DataStore {
        &self.data_store
    }

    pub fn store_info_msg(&self) -> Option<&SetStoreInfo> {
        self.set_store_info.as_ref()
    }

    pub fn store_info(&self) -> Option<&StoreInfo> {
        self.store_info_msg().map(|msg| &msg.info)
    }

    pub fn app_id(&self) -> Option<&ApplicationId> {
        self.store_info().map(|ri| &ri.application_id)
    }

    #[inline]
    pub fn query_caches(&self) -> &re_query::Caches {
        &self.query_caches
    }

    #[inline]
    pub fn resolver(&self) -> &re_query::PromiseResolver {
        &self.resolver
    }

    /// Returns `Ok(None)` if any of the required components are missing.
    #[inline]
    pub fn latest_at_archetype<A: re_types_core::Archetype>(
        &self,
        entity_path: &EntityPath,
        query: &re_data_store::LatestAtQuery,
    ) -> PromiseResult<Option<((re_log_types::TimeInt, RowId), A)>>
    where
        re_query::LatestAtResults: re_query::ToArchetype<A>,
    {
        let results = self.query_caches().latest_at(
            self.store(),
            query,
            entity_path,
            A::all_components().iter().copied(), // no generics!
        );

        use re_query::ToArchetype as _;
        match results.to_archetype(self.resolver()).flatten() {
            PromiseResult::Pending => PromiseResult::Pending,
            PromiseResult::Error(err) => {
                // Primary component has never been logged.
                if let Some(err) = err.downcast_ref::<re_query::QueryError>() {
                    if matches!(err, re_query::QueryError::PrimaryNotFound(_)) {
                        return PromiseResult::Ready(None);
                    }
                }

                // Primary component has been cleared.
                if let Some(err) = err.downcast_ref::<re_types_core::DeserializationError>() {
                    if matches!(err, re_types_core::DeserializationError::MissingData { .. }) {
                        return PromiseResult::Ready(None);
                    }
                }

                PromiseResult::Error(err)
            }
            PromiseResult::Ready(arch) => {
                PromiseResult::Ready(Some((results.compound_index, arch)))
            }
        }
    }

    /// Get the latest index and value for a given dense [`re_types_core::Component`].
    ///
    /// This assumes that the row we get from the store contains at most one instance for this
    /// component; it will log a warning otherwise.
    ///
    /// This should only be used for "mono-components" such as `Transform` and `Tensor`.
    ///
    /// This is a best-effort helper, it will merely log errors on failure.
    #[inline]
    pub fn latest_at_component<C: re_types_core::Component>(
        &self,
        entity_path: &EntityPath,
        query: &re_data_store::LatestAtQuery,
    ) -> Option<re_query::LatestAtMonoResult<C>> {
        self.query_caches().latest_at_component::<C>(
            self.store(),
            self.resolver(),
            entity_path,
            query,
        )
    }

    /// Get the latest index and value for a given dense [`re_types_core::Component`].
    ///
    /// This assumes that the row we get from the store contains at most one instance for this
    /// component; it will log a warning otherwise.
    ///
    /// This should only be used for "mono-components" such as `Transform` and `Tensor`.
    ///
    /// This is a best-effort helper, and will quietly swallow any errors.
    #[inline]
    pub fn latest_at_component_quiet<C: re_types_core::Component>(
        &self,
        entity_path: &EntityPath,
        query: &re_data_store::LatestAtQuery,
    ) -> Option<re_query::LatestAtMonoResult<C>> {
        self.query_caches().latest_at_component_quiet::<C>(
            self.store(),
            self.resolver(),
            entity_path,
            query,
        )
    }

    #[inline]
    pub fn latest_at_component_at_closest_ancestor<C: re_types_core::Component>(
        &self,
        entity_path: &EntityPath,
        query: &re_data_store::LatestAtQuery,
    ) -> Option<(EntityPath, re_query::LatestAtMonoResult<C>)> {
        self.query_caches()
            .latest_at_component_at_closest_ancestor::<C>(
                self.store(),
                self.resolver(),
                entity_path,
                query,
            )
    }

    #[inline]
    pub fn store(&self) -> &DataStore {
        &self.data_store
    }

    #[inline]
    pub fn store_kind(&self) -> StoreKind {
        self.store_id().kind
    }

    #[inline]
    pub fn store_id(&self) -> &StoreId {
        self.data_store.id()
    }

    /// If this entity db is the result of a clone, which store was it cloned from?
    ///
    /// A cloned store always gets a new unique ID.
    ///
    /// We currently only use entity db cloning for blueprints:
    /// when we activate a _default_ blueprint that was received on the wire (e.g. from a recording),
    /// we clone it and make the clone the _active_ blueprint.
    /// This means all active blueprints are clones.
    #[inline]
    pub fn cloned_from(&self) -> Option<&StoreId> {
        self.store_info().and_then(|info| info.cloned_from.as_ref())
    }

    pub fn timelines(&self) -> impl ExactSizeIterator<Item = &Timeline> {
        self.times_per_timeline().keys()
    }

    pub fn times_per_timeline(&self) -> &TimesPerTimeline {
        &self.times_per_timeline
    }

    /// Histogram of all events on the timeeline, of all entities.
    pub fn time_histogram(&self, timeline: &Timeline) -> Option<&crate::TimeHistogram> {
        self.tree().subtree.time_histogram.get(timeline)
    }

    /// Total number of static messages for any entity.
    pub fn num_static_messages(&self) -> u64 {
        self.tree.num_static_messages_recursive()
    }

    /// Returns whether a component is static.
    pub fn is_component_static(&self, component_path: &ComponentPath) -> Option<bool> {
        if let Some(entity_tree) = self.tree().subtree(component_path.entity_path()) {
            entity_tree
                .entity
                .components
                .get(&component_path.component_name)
                .map(|component_histogram| component_histogram.is_static())
        } else {
            None
        }
    }

    pub fn num_rows(&self) -> usize {
        self.data_store.num_static_rows() as usize + self.data_store.num_temporal_rows() as usize
    }

    /// Return the current `StoreGeneration`. This can be used to determine whether the
    /// database has been modified since the last time it was queried.
    #[inline]
    pub fn generation(&self) -> re_data_store::StoreGeneration {
        self.data_store.generation()
    }

    #[inline]
    pub fn last_modified_at(&self) -> web_time::Instant {
        self.last_modified_at
    }

    /// The highest `RowId` in the store,
    /// which corresponds to the last edit time.
    /// Ignores deletions.
    #[inline]
    pub fn latest_row_id(&self) -> Option<RowId> {
        self.latest_row_id
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.set_store_info.is_none() && self.num_rows() == 0
    }

    /// A sorted list of all the entity paths in this database.
    pub fn entity_paths(&self) -> Vec<&EntityPath> {
        use itertools::Itertools as _;
        self.entity_path_from_hash.values().sorted().collect()
    }

    #[inline]
    pub fn ingestion_stats(&self) -> &IngestionStatistics {
        &self.stats
    }

    #[inline]
    pub fn entity_path_from_hash(&self, entity_path_hash: &EntityPathHash) -> Option<&EntityPath> {
        self.entity_path_from_hash.get(entity_path_hash)
    }

    /// Returns `true` also for entities higher up in the hierarchy.
    #[inline]
    pub fn is_known_entity(&self, entity_path: &EntityPath) -> bool {
        self.tree.subtree(entity_path).is_some()
    }

    /// If you log `world/points`, then that is a logged entity, but `world` is not,
    /// unless you log something to `world` too.
    #[inline]
    pub fn is_logged_entity(&self, entity_path: &EntityPath) -> bool {
        self.entity_path_from_hash.contains_key(&entity_path.hash())
    }

    pub fn add(&mut self, msg: &LogMsg) -> Result<(), Error> {
        re_tracing::profile_function!();

        debug_assert_eq!(msg.store_id(), self.store_id());

        match &msg {
            LogMsg::SetStoreInfo(msg) => self.set_store_info(msg.clone()),

            LogMsg::ArrowMsg(_, arrow_msg) => {
                let table = DataTable::from_arrow_msg(arrow_msg)?;
                self.add_data_table(table)?;
            }

            LogMsg::BlueprintActivationCommand(_) => {
                // Not for us to handle
            }
        }

        Ok(())
    }

    pub fn add_data_table(&mut self, mut table: DataTable) -> Result<(), Error> {
        // TODO(#1760): Compute the size of the datacells in the batching threads on the clients.
        table.compute_all_size_bytes();

        for row in table.to_rows() {
            self.add_data_row(row?)?;
        }

        self.last_modified_at = web_time::Instant::now();

        Ok(())
    }

    /// Inserts a [`DataRow`] into the database.
    ///
    /// Updates the [`crate::EntityTree`] and applies [`ClearCascade`]s as needed.
    pub fn add_data_row(&mut self, row: DataRow) -> Result<(), Error> {
        re_tracing::profile_function!(format!("num_cells={}", row.num_cells()));

        self.register_entity_path(&row.entity_path);

        if self
            .latest_row_id
            .map_or(true, |latest| latest < row.row_id)
        {
            self.latest_row_id = Some(row.row_id);
        }

        // ## RowId duplication
        //
        // We shouldn't be attempting to retry in this instance: a duplicated RowId at this stage
        // is likely a user error.
        //
        // We only do so because, the way our 'save' feature is currently implemented in the
        // viewer can result in a single row's worth of data to be split across several insertions
        // when loading that data back (because we dump per-bucket, and RowIds get duplicated
        // across buckets).
        //
        // TODO(#1894): Remove this once the save/load process becomes RowId-driven.
        let store_event = insert_row_with_retries(
            &mut self.data_store,
            row,
            MAX_INSERT_ROW_ATTEMPTS,
            DEFAULT_INSERT_ROW_STEP_SIZE,
        )?;

        // First-pass: update our internal views by notifying them of resulting [`StoreEvent`]s.
        //
        // This might result in a [`ClearCascade`] if the events trigger one or more immediate
        // and/or pending clears.
        let original_store_events = &[store_event];
        self.times_per_timeline.on_events(original_store_events);
        self.query_caches.on_events(original_store_events);
        let clear_cascade = self.tree.on_store_additions(original_store_events);

        // Second-pass: update the [`DataStore`] by applying the [`ClearCascade`].
        //
        // This will in turn generate new [`StoreEvent`]s that our internal views need to be
        // notified of, again!
        let new_store_events = self.on_clear_cascade(clear_cascade);
        self.times_per_timeline.on_events(&new_store_events);
        self.query_caches.on_events(&new_store_events);
        let clear_cascade = self.tree.on_store_additions(&new_store_events);

        // Clears don't affect `Clear` components themselves, therefore we cannot have recursive
        // cascades, thus this whole process must stabilize after one iteration.
        if !clear_cascade.is_empty() {
            re_log::debug!(
                "recursive clear cascade detected -- might just have been logged this way"
            );
        }

        // We inform the stats last, since it measures e2e latency.
        self.stats.on_events(original_store_events);

        Ok(())
    }

    fn on_clear_cascade(&mut self, clear_cascade: ClearCascade) -> Vec<StoreEvent> {
        let mut store_events = Vec::new();

        // Create the empty cells to be inserted.
        //
        // Reminder: these are the [`RowId`]s of the `Clear` components that triggered the
        // cascade, they are not unique and may be shared across many entity paths.
        let mut to_be_inserted =
            BTreeMap::<RowId, BTreeMap<EntityPath, (TimePoint, Vec<DataCell>)>>::default();
        for (row_id, per_entity) in clear_cascade.to_be_cleared {
            for (entity_path, (timepoint, component_paths)) in per_entity {
                let per_entity = to_be_inserted.entry(row_id).or_default();
                let (cur_timepoint, cells) = per_entity.entry(entity_path).or_default();

                *cur_timepoint = timepoint.union_max(cur_timepoint);
                for component_path in component_paths {
                    if let Some(data_type) = self
                        .data_store
                        .lookup_datatype(&component_path.component_name)
                    {
                        cells.push(DataCell::from_arrow_empty(
                            component_path.component_name,
                            data_type.clone(),
                        ));
                    }
                }
            }
        }

        for (row_id, per_entity) in to_be_inserted {
            let mut row_id = row_id;
            for (entity_path, (timepoint, cells)) in per_entity {
                // NOTE: It is important we insert all those empty components using a single row (id)!
                // 1. It'll be much more efficient when querying that data back.
                // 2. Otherwise we will end up with a flaky row ordering, as we have no way to tie-break
                //    these rows! This flaky ordering will in turn leak through the public
                //    API (e.g. range queries)!
                match DataRow::from_cells(row_id, timepoint.clone(), entity_path, cells) {
                    Ok(row) => {
                        let res = insert_row_with_retries(
                            &mut self.data_store,
                            row,
                            MAX_INSERT_ROW_ATTEMPTS,
                            DEFAULT_INSERT_ROW_STEP_SIZE,
                        );

                        match res {
                            Ok(store_event) => {
                                row_id = store_event.row_id.next();
                                store_events.push(store_event);
                            }
                            Err(err) => {
                                re_log::error_once!(
                                    "Failed to propagate EntityTree cascade: {err}"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        re_log::error_once!("Failed to propagate EntityTree cascade: {err}");
                    }
                }
            }
        }

        store_events
    }

    fn register_entity_path(&mut self, entity_path: &EntityPath) {
        self.entity_path_from_hash
            .entry(entity_path.hash())
            .or_insert_with(|| entity_path.clone());
    }

    pub fn set_store_info(&mut self, store_info: SetStoreInfo) {
        self.set_store_info = Some(store_info);
    }

    pub fn gc_everything_but_the_latest_row(&mut self) {
        re_tracing::profile_function!();

        self.gc(&GarbageCollectionOptions {
            target: re_data_store::GarbageCollectionTarget::Everything,
            protect_latest: 1, // TODO(jleibs): Bump this after we have an undo buffer
            purge_empty_tables: true,
            dont_protect: [
                re_types_core::components::ClearIsRecursive::name(),
                re_types_core::archetypes::Clear::indicator().name(),
            ]
            .into_iter()
            .collect(),
            enable_batching: false,
            time_budget: DEFAULT_GC_TIME_BUDGET,
        });
    }

    /// Free up some RAM by forgetting the older parts of all timelines.
    pub fn purge_fraction_of_ram(&mut self, fraction_to_purge: f32) {
        re_tracing::profile_function!();

        assert!((0.0..=1.0).contains(&fraction_to_purge));
        self.gc(&GarbageCollectionOptions {
            target: re_data_store::GarbageCollectionTarget::DropAtLeastFraction(
                fraction_to_purge as _,
            ),
            protect_latest: 1,
            purge_empty_tables: false,
            dont_protect: Default::default(),
            enable_batching: false,
            time_budget: DEFAULT_GC_TIME_BUDGET,
        });
    }

    pub fn gc(&mut self, gc_options: &GarbageCollectionOptions) {
        re_tracing::profile_function!();

        let (store_events, stats_diff) = self.data_store.gc(gc_options);

        re_log::trace!(
            num_row_ids_dropped = store_events.len(),
            size_bytes_dropped = re_format::format_bytes(stats_diff.total.num_bytes as _),
            "purged datastore"
        );

        self.on_store_deletions(&store_events);
    }

    fn on_store_deletions(&mut self, store_events: &[StoreEvent]) {
        re_tracing::profile_function!();

        let Self {
            data_source: _,
            set_store_info: _,
            last_modified_at: _,
            latest_row_id: _,
            entity_path_from_hash: _,
            times_per_timeline,
            tree,
            data_store: _,
            resolver: _,
            query_caches,
            stats: _,
        } = self;

        times_per_timeline.on_events(store_events);
        query_caches.on_events(store_events);

        let store_events = store_events.iter().collect_vec();
        let compacted = CompactedStoreEvents::new(&store_events);
        tree.on_store_deletions(&store_events, &compacted);
    }

    /// Key used for sorting recordings in the UI.
    pub fn sort_key(&self) -> impl Ord + '_ {
        self.store_info()
            .map(|info| (info.application_id.0.as_str(), info.started))
    }

    /// Export the contents of the current database to a sequence of messages.
    ///
    /// If `time_selection` is specified, then only data for that specific timeline over that
    /// specific time range will be accounted for.
    pub fn to_messages(
        &self,
        time_selection: Option<(Timeline, ResolvedTimeRangeF)>,
    ) -> DataTableResult<Vec<LogMsg>> {
        re_tracing::profile_function!();

        self.store().sort_indices_if_needed();

        let set_store_info_msg = self
            .store_info_msg()
            .map(|msg| Ok(LogMsg::SetStoreInfo(msg.clone())));

        let time_filter = time_selection.map(|(timeline, range)| {
            (
                timeline,
                ResolvedTimeRange::new(range.min.floor(), range.max.ceil()),
            )
        });

        let data_messages = self.store().to_data_tables(time_filter).map(|table| {
            table
                .to_arrow_msg()
                .map(|msg| LogMsg::ArrowMsg(self.store_id().clone(), msg))
        });

        // If this is a blueprint, make sure to include the `BlueprintActivationCommand` message.
        // We generally use `to_messages` to export a blueprint via "save". In that
        // case, we want to make the blueprint active and default when it's reloaded.
        // TODO(jleibs): Coupling this with the stored file instead of injecting seems
        // architecturally weird. Would be great if we didn't need this in `.rbl` files
        // at all.
        let blueprint_ready = if self.store_kind() == StoreKind::Blueprint {
            let activate_cmd =
                re_log_types::BlueprintActivationCommand::make_active(self.store_id().clone());

            itertools::Either::Left(std::iter::once(Ok(activate_cmd.into())))
        } else {
            itertools::Either::Right(std::iter::empty())
        };

        let messages: Result<Vec<_>, _> = set_store_info_msg
            .into_iter()
            .chain(data_messages)
            .chain(blueprint_ready)
            .collect();

        messages
    }

    /// Make a clone of this [`EntityDb`], assigning it a new [`StoreId`].
    pub fn clone_with_new_id(&self, new_id: StoreId) -> Result<EntityDb, Error> {
        re_tracing::profile_function!();

        self.store().sort_indices_if_needed();

        let mut new_db = EntityDb::new(new_id.clone());

        new_db.last_modified_at = self.last_modified_at;
        new_db.latest_row_id = self.latest_row_id;

        // We do NOT clone the `data_source`, because the reason we clone an entity db
        // is so that we can modify it, and then it would be wrong to say its from the same source.
        // Specifically: if we load a blueprint from an `.rdd`, then modify it heavily and save it,
        // it would be wrong to claim that this was the blueprint from that `.rrd`,
        // and it would confuse the user.
        // TODO(emilk): maybe we should use a special `Cloned` data source,
        // wrapping either the original source, the original StoreId, or both.

        if let Some(store_info) = self.store_info() {
            let mut new_info = store_info.clone();
            new_info.store_id = new_id;
            new_info.cloned_from = Some(self.store_id().clone());

            new_db.set_store_info(SetStoreInfo {
                row_id: RowId::new(),
                info: new_info,
            });
        }

        for row in self.store().to_rows()? {
            new_db.add_data_row(row)?;
        }

        Ok(new_db)
    }
}

impl re_types_core::SizeBytes for EntityDb {
    fn heap_size_bytes(&self) -> u64 {
        // TODO(emilk): size of entire EntityDb, including secondary indices etc
        self.data_store.heap_size_bytes()
    }
}

// ----------------------------------------------------------------------------

pub struct IngestionStatistics {
    store_id: StoreId,
    e2e_latency_sec_history: Mutex<emath::History<f32>>,
}

impl StoreSubscriber for IngestionStatistics {
    fn name(&self) -> String {
        "rerun.testing.store_subscribers.IngestionStatistics".into()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn on_events(&mut self, events: &[StoreEvent]) {
        for event in events {
            if event.store_id == self.store_id {
                self.on_new_row_id(event.row_id);
            }
        }
    }
}

impl IngestionStatistics {
    pub fn new(store_id: StoreId) -> Self {
        let min_samples = 0; // 0: we stop displaying e2e latency if input stops
        let max_samples = 1024; // don't waste too much memory on this - we just need enough to get a good average
        let max_age = 1.0; // don't keep too long of a rolling average, or the stats get outdated.
        Self {
            store_id,
            e2e_latency_sec_history: Mutex::new(emath::History::new(
                min_samples..max_samples,
                max_age,
            )),
        }
    }

    fn on_new_row_id(&mut self, row_id: RowId) {
        if let Ok(duration_since_epoch) = web_time::SystemTime::UNIX_EPOCH.elapsed() {
            let nanos_since_epoch = duration_since_epoch.as_nanos() as u64;

            // This only makes sense if the clocks are very good, i.e. if the recording was on the same machine!
            if let Some(nanos_since_log) =
                nanos_since_epoch.checked_sub(row_id.nanoseconds_since_epoch())
            {
                let now = nanos_since_epoch as f64 / 1e9;
                let sec_since_log = nanos_since_log as f32 / 1e9;

                self.e2e_latency_sec_history.lock().add(now, sec_since_log);
            }
        }
    }

    /// What is the mean latency between the time data was logged in the SDK and the time it was ingested?
    ///
    /// This is based on the clocks of the viewer and the SDK being in sync,
    /// so if the recording was done on another machine, this is likely very inaccurate.
    pub fn current_e2e_latency_sec(&self) -> Option<f32> {
        let mut e2e_latency_sec_history = self.e2e_latency_sec_history.lock();

        if let Ok(duration_since_epoch) = web_time::SystemTime::UNIX_EPOCH.elapsed() {
            let nanos_since_epoch = duration_since_epoch.as_nanos() as u64;
            let now = nanos_since_epoch as f64 / 1e9;
            e2e_latency_sec_history.flush(now); // make sure the average is up-to-date.
        }

        e2e_latency_sec_history.average()
    }
}
