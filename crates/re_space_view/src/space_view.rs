use itertools::{FoldWhile, Itertools};
use re_entity_db::{external::re_query::PromiseResult, EntityProperties};
use re_types::SpaceViewClassIdentifier;

use crate::SpaceViewContents;
use re_data_store::LatestAtQuery;
use re_entity_db::{EntityDb, EntityPath, EntityPropertiesComponent, EntityPropertyMap};
use re_log_types::{DataRow, EntityPathSubs, RowId, Timeline};
use re_types::{
    blueprint::{
        archetypes::{self as blueprint_archetypes},
        components::{SpaceViewOrigin, Visible},
    },
    components::Name,
};
use re_types_core::archetypes::Clear;
use re_types_core::Archetype as _;
use re_viewer_context::{
    ContentsName, DataResult, PerSystemEntities, QueryRange, RecommendedSpaceView, SpaceViewClass,
    SpaceViewClassRegistry, SpaceViewId, SpaceViewState, StoreContext, SystemCommand,
    SystemCommandSender as _, SystemExecutionOutput, ViewQuery, ViewerContext,
};

/// A view of a space.
///
/// Note: [`SpaceViewBlueprint`] doesn't implement Clone because it stores an internal
/// uuid used for identifying the path of its data in the blueprint store. It's ambiguous
/// whether the intent is for a clone to write to the same place.
///
/// If you want a new space view otherwise identical to an existing one, use
/// `re_viewport::ViewportBlueprint::duplicate_space_view`.
pub struct SpaceViewBlueprint {
    pub id: SpaceViewId,
    pub display_name: Option<String>,
    class_identifier: SpaceViewClassIdentifier,

    /// The "anchor point" of this space view.
    /// The transform at this path forms the reference point for all scene->world transforms in this space view.
    /// I.e. the position of this entity path in space forms the origin of the coordinate system in this space view.
    /// Furthermore, this is the primary indicator for heuristics on what entities we show in this space view.
    pub space_origin: EntityPath,

    /// The content of this space view as defined by its queries.
    pub contents: SpaceViewContents,

    /// True if this space view is visible in the UI.
    pub visible: bool,

    /// Pending blueprint writes for nested components from duplicate.
    pending_writes: Vec<DataRow>,
}

impl SpaceViewBlueprint {
    /// Creates a new [`SpaceViewBlueprint`] with a single [`SpaceViewContents`].
    ///
    /// This [`SpaceViewBlueprint`] is ephemeral. If you want to make it permanent you
    /// must call [`Self::save_to_blueprint_store`].
    pub fn new(
        space_view_class: SpaceViewClassIdentifier,
        recommended: RecommendedSpaceView,
    ) -> Self {
        let id = SpaceViewId::random();

        Self {
            display_name: None,
            class_identifier: space_view_class,
            id,
            space_origin: recommended.origin,
            contents: SpaceViewContents::new(id, space_view_class, recommended.query_filter),
            visible: true,
            pending_writes: Default::default(),
        }
    }

    /// Placeholder name displayed in the UI if the user hasn't explicitly named the space view.
    pub fn missing_name_placeholder(&self) -> String {
        let entity_path = self
            .space_origin
            .iter()
            .rev()
            .fold_while(String::new(), |acc, path| {
                if acc.len() > 10 {
                    FoldWhile::Done(format!("…/{acc}"))
                } else {
                    FoldWhile::Continue(format!(
                        "{}{}{}",
                        path.ui_string(),
                        if acc.is_empty() { "" } else { "/" },
                        acc
                    ))
                }
            })
            .into_inner();

        if entity_path.is_empty() {
            "/".to_owned()
        } else {
            entity_path
        }
    }

    /// Returns this space view's display name
    ///
    /// When returning [`ContentsName::Placeholder`], the UI should display the resulting name using
    /// `re_ui::LabelStyle::Unnamed`.
    pub fn display_name_or_default(&self) -> ContentsName {
        self.display_name.clone().map_or_else(
            || ContentsName::Placeholder(self.missing_name_placeholder()),
            ContentsName::Named,
        )
    }

    /// Attempt to load a [`SpaceViewBlueprint`] from the blueprint store.
    pub fn try_from_db(
        id: SpaceViewId,
        blueprint_db: &EntityDb,
        query: &LatestAtQuery,
    ) -> Option<Self> {
        re_tracing::profile_function!();

        let blueprint_archetypes::SpaceViewBlueprint {
            display_name,
            class_identifier,
            space_origin,
            visible,
        } = match blueprint_db.latest_at_archetype(&id.as_entity_path(), query) {
            PromiseResult::Pending => {
                // TODO(#5607): what should happen if the promise is still pending?
                None
            }
            PromiseResult::Ready(arch) => arch.map(|(_, arch)| arch),
            PromiseResult::Error(err) => {
                if cfg!(debug_assertions) {
                    re_log::error!("Failed to load SpaceView blueprint: {err}.");
                } else {
                    re_log::debug!("Failed to load SpaceView blueprint: {err}.");
                }
                None
            }
        }?;

        let space_origin = space_origin.map_or_else(EntityPath::root, |origin| origin.0.into());
        let class_identifier: SpaceViewClassIdentifier = class_identifier.0.as_str().into();
        let display_name = display_name.map(|v| v.0.to_string());

        let space_env = EntityPathSubs::new_with_origin(&space_origin);

        let content = SpaceViewContents::from_db_or_default(
            id,
            blueprint_db,
            query,
            class_identifier,
            &space_env,
        );
        let visible = visible.map_or(true, |v| v.0);

        Some(Self {
            id,
            display_name,
            class_identifier,
            space_origin,
            contents: content,
            visible,
            pending_writes: Default::default(),
        })
    }

    /// Persist the entire [`SpaceViewBlueprint`] to the blueprint store.
    ///
    /// This only needs to be called if the [`SpaceViewBlueprint`] was created with [`Self::new`].
    ///
    /// Otherwise, incremental calls to `set_` functions will write just the necessary component
    /// update directly to the store.
    pub fn save_to_blueprint_store(&self, ctx: &ViewerContext<'_>) {
        let timepoint = ctx.store_context.blueprint_timepoint_for_writes();

        let Self {
            id,
            display_name,
            class_identifier,
            space_origin,
            contents,
            visible,
            pending_writes,
        } = self;

        let mut arch = blueprint_archetypes::SpaceViewBlueprint::new(class_identifier.as_str())
            .with_space_origin(space_origin)
            .with_visible(*visible);

        if let Some(display_name) = display_name {
            arch = arch.with_display_name(display_name.clone());
        }

        // Start with the pending writes, which explicitly filtered out the `SpaceViewBlueprint`
        // components from the top level.
        let mut deltas = pending_writes.clone();

        // Add all the additional components from the archetype
        if let Ok(row) =
            DataRow::from_archetype(RowId::new(), timepoint.clone(), id.as_entity_path(), &arch)
        {
            deltas.push(row);
        }

        contents.save_to_blueprint_store(ctx);

        ctx.command_sender
            .send_system(SystemCommand::UpdateBlueprint(
                ctx.store_context.blueprint.store_id().clone(),
                deltas,
            ));
    }

    /// Creates a new [`SpaceViewBlueprint`] with the same contents, but a different [`SpaceViewId`]
    ///
    /// Also duplicates all the queries in the space view.
    pub fn duplicate(&self, store_context: &StoreContext<'_>, query: &LatestAtQuery) -> Self {
        let mut pending_writes = Vec::new();
        let blueprint = store_context.blueprint;

        let current_path = self.entity_path();
        let new_id = SpaceViewId::random();
        let new_path = new_id.as_entity_path();

        // Create pending write operations to duplicate the entire subtree
        // TODO(jleibs): This should be a helper somewhere.
        if let Some(tree) = blueprint.tree().subtree(&current_path) {
            tree.visit_children_recursively(&mut |path, info| {
                let sub_path: EntityPath = new_path
                    .iter()
                    .chain(&path[current_path.len()..])
                    .cloned()
                    .collect();

                if let Ok(row) = DataRow::from_cells(
                    RowId::new(),
                    store_context.blueprint_timepoint_for_writes(),
                    sub_path,
                    info.components
                        .keys()
                        // It's important that we don't include the SpaceViewBlueprint's components
                        // since those will be updated separately and may contain different data.
                        .filter(|component| {
                            *path != current_path
                                || !blueprint_archetypes::SpaceViewBlueprint::all_components()
                                    .contains(component)
                        })
                        .filter_map(|component| {
                            blueprint
                                .store()
                                .latest_at(query, path, *component, &[*component])
                                .and_then(|(_, _, cells)| cells[0].clone())
                        }),
                ) {
                    if row.num_cells() > 0 {
                        pending_writes.push(row);
                    }
                }
            });
        }

        // SpaceViewContents is saved as an archetype in the space view's entity hierarchy.
        // This means, that the above already copied the space view contents!
        let contents = SpaceViewContents::new(
            new_id,
            self.class_identifier,
            self.contents.entity_path_filter.clone(),
        );

        Self {
            id: new_id,
            display_name: self.display_name.clone(),
            class_identifier: self.class_identifier,
            space_origin: self.space_origin.clone(),
            contents,
            visible: self.visible,
            pending_writes,
        }
    }

    pub fn clear(&self, ctx: &ViewerContext<'_>) {
        let clear = Clear::recursive();
        ctx.save_blueprint_component(&self.entity_path(), &clear.is_recursive);
    }

    #[inline]
    pub fn set_display_name(&self, ctx: &ViewerContext<'_>, name: Option<String>) {
        if name != self.display_name {
            match name {
                Some(name) => {
                    let component = Name(name.into());
                    ctx.save_blueprint_component(&self.entity_path(), &component);
                }
                None => {
                    ctx.save_empty_blueprint_component::<Name>(&self.entity_path());
                }
            }
        }
    }

    #[inline]
    pub fn set_origin(&self, ctx: &ViewerContext<'_>, origin: &EntityPath) {
        if origin != &self.space_origin {
            let component = SpaceViewOrigin(origin.into());
            ctx.save_blueprint_component(&self.entity_path(), &component);
        }
    }

    #[inline]
    pub fn set_visible(&self, ctx: &ViewerContext<'_>, visible: bool) {
        if visible != self.visible {
            let component = Visible(visible);
            ctx.save_blueprint_component(&self.entity_path(), &component);
        }
    }

    pub fn class_identifier(&self) -> &SpaceViewClassIdentifier {
        &self.class_identifier
    }

    pub fn class<'a>(
        &self,
        space_view_class_registry: &'a re_viewer_context::SpaceViewClassRegistry,
    ) -> &'a dyn SpaceViewClass {
        space_view_class_registry.get_class_or_log_error(&self.class_identifier)
    }

    pub fn on_frame_start(
        &self,
        ctx: &ViewerContext<'_>,
        view_state: &mut dyn SpaceViewState,
        auto_properties: &mut EntityPropertyMap,
    ) {
        let query_result = ctx.lookup_query_result(self.id).clone();

        let mut per_system_entities = PerSystemEntities::default();
        {
            re_tracing::profile_scope!("per_system_data_results");

            query_result.tree.visit(&mut |node| {
                for system in &node.data_result.visualizers {
                    per_system_entities
                        .entry(*system)
                        .or_default()
                        .insert(node.data_result.entity_path.clone());
                }
                true
            });
        }

        self.class(ctx.space_view_class_registry).on_frame_start(
            ctx,
            view_state,
            &per_system_entities,
            auto_properties,
        );
    }

    pub fn scene_ui(
        &self,
        view_state: &mut dyn SpaceViewState,
        ctx: &ViewerContext<'_>,
        ui: &mut egui::Ui,
        query: &ViewQuery<'_>,
        system_output: SystemExecutionOutput,
    ) {
        re_tracing::profile_function!();

        let class = self.class(ctx.space_view_class_registry);

        let props = self.legacy_properties(ctx.store_context.blueprint, ctx.blueprint_query);

        ui.scope(|ui| {
            class
                .ui(ctx, ui, view_state, &props, query, system_output)
                .unwrap_or_else(|err| {
                    re_log::error!(
                        "Error in space view UI (class: {}, display name: {}): {err}",
                        self.class_identifier,
                        class.display_name(),
                    );
                });
        });
    }

    #[inline]
    pub fn entity_path(&self) -> EntityPath {
        self.id.as_entity_path()
    }

    /// Legacy `EntityProperties` used by a hand ful of view properties that aren't blueprint view properties yet.
    pub fn legacy_properties(
        &self,
        blueprint: &EntityDb,
        blueprint_query: &LatestAtQuery,
    ) -> EntityProperties {
        let base_override_root = self.entity_path();
        let individual_override_path =
            base_override_root.join(&DataResult::INDIVIDUAL_OVERRIDES_PREFIX.into());

        blueprint
            .latest_at_component_quiet::<EntityPropertiesComponent>(
                &individual_override_path,
                blueprint_query,
            )
            .map(|result| result.value.0)
            .unwrap_or_default()
    }

    pub fn save_legacy_properties(&self, ctx: &ViewerContext<'_>, props: EntityProperties) {
        let base_override_root = self.entity_path();
        let individual_override_path =
            base_override_root.join(&DataResult::INDIVIDUAL_OVERRIDES_PREFIX.into());

        ctx.save_blueprint_component(&individual_override_path, &EntityPropertiesComponent(props));
    }

    pub fn query_range(
        &self,
        blueprint: &EntityDb,
        blueprint_query: &LatestAtQuery,
        active_timeline: &Timeline,
        space_view_class_registry: &SpaceViewClassRegistry,
    ) -> QueryRange {
        // Visual time range works with regular overrides for the most part but it's a bit special:
        // * we need it for all entities unconditionally
        // * default does not vary per visualizer
        // * can't be specified in the data store
        // Here, we query the visual time range that serves as the default for all entities in this space.
        let (visible_time_range_archetype, _) = crate::query_view_property::<
            blueprint_archetypes::VisibleTimeRanges,
        >(self.id, blueprint, blueprint_query);

        let visible_time_range_archetype = visible_time_range_archetype.ok().flatten();

        let time_range = visible_time_range_archetype
            .as_ref()
            .and_then(|arch| arch.range_for_timeline(active_timeline.name().as_str()));
        time_range.map_or_else(
            || {
                let space_view_class =
                    space_view_class_registry.get_class_or_log_error(&self.class_identifier);
                space_view_class.default_query_range()
            },
            |time_range| QueryRange::TimeRange(time_range.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::data_query::{DataQuery, PropertyResolver};
    use re_entity_db::{EntityDb, EntityProperties, EntityPropertiesComponent};
    use re_log_types::{
        example_components::{MyColor, MyLabel, MyPoint},
        DataCell, DataRow, RowId, StoreId, StoreKind, TimePoint,
    };
    use re_types::{archetypes::Points3D, ComponentBatch, ComponentName, Loggable as _};
    use re_viewer_context::{
        blueprint_timeline, IndicatedEntities, OverridePath, PerVisualizer, SpaceViewClassRegistry,
        StoreContext, VisualizableEntities,
    };
    use std::collections::HashMap;

    use super::*;

    fn save_override(props: EntityProperties, path: &EntityPath, store: &mut EntityDb) {
        let component = EntityPropertiesComponent(props);
        let row = DataRow::from_cells1_sized(
            RowId::new(),
            path.clone(),
            TimePoint::default(),
            DataCell::from([component]),
        )
        .unwrap();

        store.add_data_row(row).unwrap();
    }

    #[test]
    fn test_entity_properties() {
        let space_view_class_registry = SpaceViewClassRegistry::default();
        let timeline = Timeline::new("time", re_log_types::TimeType::Time);
        let mut recording = EntityDb::new(StoreId::random(re_log_types::StoreKind::Recording));
        let mut blueprint = EntityDb::new(StoreId::random(re_log_types::StoreKind::Blueprint));
        let legacy_auto_properties = EntityPropertyMap::default();

        let points = Points3D::new(vec![[1.0, 2.0, 3.0]]);

        for path in [
            "parent".into(),
            "parent/skip/child1".into(),
            "parent/skip/child2".into(),
        ] {
            let row =
                DataRow::from_archetype(RowId::new(), TimePoint::default(), path, &points).unwrap();
            recording.add_data_row(row).ok();
        }

        let recommended = RecommendedSpaceView::new(
            EntityPath::root(),
            ["+ parent", "+ parent/skip/child1", "+ parent/skip/child2"],
        );

        let space_view = SpaceViewBlueprint::new("3D".into(), recommended);

        let mut visualizable_entities = PerVisualizer::<VisualizableEntities>::default();
        visualizable_entities
            .0
            .entry("Points3D".into())
            .or_insert_with(|| {
                VisualizableEntities(
                    [
                        EntityPath::from("parent"),
                        EntityPath::from("parent/skipped/child1"),
                    ]
                    .into_iter()
                    .collect(),
                )
            });
        let indicated_entities_per_visualizer = PerVisualizer::<IndicatedEntities>(
            visualizable_entities
                .0
                .iter()
                .map(|(id, entities)| (*id, IndicatedEntities(entities.iter().cloned().collect())))
                .collect(),
        );

        let blueprint_query = LatestAtQuery::latest(blueprint_timeline());
        let contents = &space_view.contents;

        let resolver = contents.build_resolver(
            &space_view_class_registry,
            &space_view,
            &visualizable_entities,
            &indicated_entities_per_visualizer,
        );

        // No overrides set. Everybody has default values.
        {
            let ctx = StoreContext {
                app_id: re_log_types::ApplicationId::unknown(),
                blueprint: &blueprint,
                default_blueprint: None,
                recording: &recording,
                bundle: &Default::default(),
                hub: &re_viewer_context::StoreHub::test_hub(),
            };

            let mut query_result = contents.execute_query(&ctx, &visualizable_entities);
            resolver.update_overrides(
                &blueprint,
                &blueprint_query,
                &timeline,
                &space_view_class_registry,
                &legacy_auto_properties,
                &mut query_result,
            );

            let parent = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent"))
                .unwrap();
            let child1 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child1"))
                .unwrap();
            let child2 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child2"))
                .unwrap();

            for result in [parent, child1, child2] {
                assert_eq!(
                    result.accumulated_properties(),
                    &EntityProperties::default(),
                );
            }

            // Now, override interactive on parent individually.
            let mut overrides = parent.individual_properties().cloned().unwrap_or_default();
            overrides.interactive = false;

            save_override(
                overrides,
                parent.individual_override_path().unwrap(),
                &mut blueprint,
            );
        }

        // Parent is not interactive, but children are
        {
            let ctx = StoreContext {
                app_id: re_log_types::ApplicationId::unknown(),
                blueprint: &blueprint,
                default_blueprint: None,
                recording: &recording,
                bundle: &Default::default(),
                hub: &re_viewer_context::StoreHub::test_hub(),
            };

            let mut query_result = contents.execute_query(&ctx, &visualizable_entities);
            resolver.update_overrides(
                &blueprint,
                &blueprint_query,
                &timeline,
                &space_view_class_registry,
                &legacy_auto_properties,
                &mut query_result,
            );

            let parent_group = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent"))
                .unwrap();
            let parent = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent"))
                .unwrap();
            let child1 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child1"))
                .unwrap();
            let child2 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child2"))
                .unwrap();

            assert!(!parent.accumulated_properties().interactive);

            for result in [child1, child2] {
                assert!(result.accumulated_properties().interactive);
            }

            // Override interactivity on parent recursively.
            let mut overrides = parent_group
                .individual_properties()
                .cloned()
                .unwrap_or_default();
            overrides.interactive = false;

            save_override(
                overrides,
                parent_group.recursive_override_path().unwrap(),
                &mut blueprint,
            );
        }

        // Nobody is interactive
        {
            let ctx = StoreContext {
                app_id: re_log_types::ApplicationId::unknown(),
                blueprint: &blueprint,
                default_blueprint: None,
                recording: &recording,
                bundle: &Default::default(),
                hub: &re_viewer_context::StoreHub::test_hub(),
            };

            let mut query_result = contents.execute_query(&ctx, &visualizable_entities);
            resolver.update_overrides(
                &blueprint,
                &blueprint_query,
                &timeline,
                &space_view_class_registry,
                &legacy_auto_properties,
                &mut query_result,
            );

            let parent = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent"))
                .unwrap();
            let child1 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child1"))
                .unwrap();
            let child2 = query_result
                .tree
                .lookup_result_by_path(&EntityPath::from("parent/skip/child2"))
                .unwrap();

            for result in [parent, child1, child2] {
                assert!(!result.accumulated_properties().interactive);
            }
        }
    }

    #[test]
    fn test_component_overrides() {
        let space_view_class_registry = SpaceViewClassRegistry::default();
        let timeline = Timeline::new("time", re_log_types::TimeType::Time);
        let legacy_auto_properties = EntityPropertyMap::default();
        let mut recording = EntityDb::new(StoreId::random(re_log_types::StoreKind::Recording));
        let mut visualizable_entities_per_visualizer =
            PerVisualizer::<VisualizableEntities>::default();

        // Set up a store DB with some entities.
        {
            let entity_paths: Vec<EntityPath> =
                ["parent", "parent/skipped/grandchild", "parent/child"]
                    .into_iter()
                    .map(Into::into)
                    .collect();
            for entity_path in &entity_paths {
                let row = DataRow::from_component_batches(
                    RowId::new(),
                    TimePoint::default(),
                    entity_path.clone(),
                    [&[MyPoint::new(1.0, 2.0)] as _],
                )
                .unwrap();
                recording.add_data_row(row).unwrap();
            }

            // All of them are visualizable with some arbitrary visualizer.
            visualizable_entities_per_visualizer
                .0
                .entry("Points3D".into())
                .or_insert_with(|| VisualizableEntities(entity_paths.into_iter().collect()));
        }

        // Basic blueprint - a single space view that queries everything.
        let space_view = SpaceViewBlueprint::new("3D".into(), RecommendedSpaceView::root());
        let individual_override_root = space_view
            .contents
            .blueprint_entity_path
            .join(&DataResult::INDIVIDUAL_OVERRIDES_PREFIX.into());
        let recursive_override_root = space_view
            .contents
            .blueprint_entity_path
            .join(&DataResult::RECURSIVE_OVERRIDES_PREFIX.into());

        // Things needed to resolve properties:
        let indicated_entities_per_visualizer = PerVisualizer::<IndicatedEntities>::default(); // Don't care about indicated entities.
        let resolver = space_view.contents.build_resolver(
            &space_view_class_registry,
            &space_view,
            &visualizable_entities_per_visualizer,
            &indicated_entities_per_visualizer,
        );

        struct Scenario {
            recursive_overrides: Vec<(EntityPath, Box<dyn ComponentBatch>)>,
            individual_overrides: Vec<(EntityPath, Box<dyn ComponentBatch>)>,
            expected_overrides: HashMap<EntityPath, HashMap<ComponentName, EntityPath>>,
        }

        let scenarios: Vec<Scenario> = vec![
            // No overrides.
            Scenario {
                recursive_overrides: Vec::new(),
                individual_overrides: Vec::new(),
                expected_overrides: HashMap::default(),
            },
            // Recursive override at parent entity.
            Scenario {
                recursive_overrides: vec![(
                    "parent".into(),
                    Box::new(MyLabel("parent_override".to_owned())),
                )],
                individual_overrides: Vec::new(),
                expected_overrides: HashMap::from([
                    (
                        "parent".into(),
                        HashMap::from([(
                            MyLabel::name(),
                            recursive_override_root.join(&"parent".into()),
                        )]),
                    ),
                    (
                        "parent/skipped".into(),
                        HashMap::from([(
                            MyLabel::name(),
                            recursive_override_root.join(&"parent".into()),
                        )]),
                    ),
                    (
                        "parent/skipped/grandchild".into(),
                        HashMap::from([(
                            MyLabel::name(),
                            recursive_override_root.join(&"parent".into()),
                        )]),
                    ),
                    (
                        "parent/child".into(),
                        HashMap::from([(
                            MyLabel::name(),
                            recursive_override_root.join(&"parent".into()),
                        )]),
                    ),
                ]),
            },
            // Set a single individual.
            Scenario {
                recursive_overrides: Vec::new(),
                individual_overrides: vec![(
                    "parent".into(),
                    Box::new(MyLabel("parent_individual".to_owned())),
                )],
                expected_overrides: HashMap::from([(
                    "parent".into(),
                    HashMap::from([(
                        MyLabel::name(),
                        individual_override_root.join(&"parent".into()),
                    )]),
                )]),
            },
            // Recursive override, partially shadowed by individual.
            Scenario {
                recursive_overrides: vec![
                    (
                        "parent/skipped".into(),
                        Box::new(MyLabel("parent_individual".to_owned())),
                    ),
                    (
                        "parent/skipped".into(),
                        Box::new(MyColor::from_rgb(0, 1, 2)),
                    ),
                ],
                individual_overrides: vec![(
                    "parent/skipped/grandchild".into(),
                    Box::new(MyColor::from_rgb(1, 2, 3)),
                )],
                expected_overrides: HashMap::from([
                    (
                        "parent/skipped".into(),
                        HashMap::from([
                            (
                                MyLabel::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                            (
                                MyColor::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                        ]),
                    ),
                    (
                        "parent/skipped/grandchild".into(),
                        HashMap::from([
                            (
                                MyLabel::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                            (
                                MyColor::name(),
                                individual_override_root.join(&"parent/skipped/grandchild".into()),
                            ),
                        ]),
                    ),
                ]),
            },
            // Recursive override, partially shadowed by another recursive override.
            Scenario {
                recursive_overrides: vec![
                    (
                        "parent/skipped".into(),
                        Box::new(MyLabel("parent_individual".to_owned())),
                    ),
                    (
                        "parent/skipped".into(),
                        Box::new(MyColor::from_rgb(0, 1, 2)),
                    ),
                    (
                        "parent/skipped/grandchild".into(),
                        Box::new(MyColor::from_rgb(3, 2, 1)),
                    ),
                ],
                individual_overrides: Vec::new(),
                expected_overrides: HashMap::from([
                    (
                        "parent/skipped".into(),
                        HashMap::from([
                            (
                                MyLabel::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                            (
                                MyColor::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                        ]),
                    ),
                    (
                        "parent/skipped/grandchild".into(),
                        HashMap::from([
                            (
                                MyLabel::name(),
                                recursive_override_root.join(&"parent/skipped".into()),
                            ),
                            (
                                MyColor::name(),
                                recursive_override_root.join(&"parent/skipped/grandchild".into()),
                            ),
                        ]),
                    ),
                ]),
            },
        ];

        for (
            i,
            Scenario {
                recursive_overrides,
                individual_overrides,
                expected_overrides,
            },
        ) in scenarios.into_iter().enumerate()
        {
            let mut blueprint = EntityDb::new(StoreId::random(re_log_types::StoreKind::Blueprint));
            let mut add_to_blueprint = |path: &EntityPath, batch: &dyn ComponentBatch| {
                let row = DataRow::from_component_batches(
                    RowId::new(),
                    TimePoint::default(),
                    path.clone(),
                    std::iter::once(batch),
                )
                .unwrap();
                blueprint.add_data_row(row).unwrap();
            };

            // log individual and override components as instructed.
            for (entity_path, batch) in recursive_overrides {
                add_to_blueprint(&recursive_override_root.join(&entity_path), batch.as_ref());
            }
            for (entity_path, batch) in individual_overrides {
                add_to_blueprint(&individual_override_root.join(&entity_path), batch.as_ref());
            }

            // Set up a store query and update the overrides.
            let ctx = StoreContext {
                app_id: re_log_types::ApplicationId::unknown(),
                blueprint: &blueprint,
                default_blueprint: None,
                recording: &recording,
                bundle: &Default::default(),
                hub: &re_viewer_context::StoreHub::test_hub(),
            };
            let mut query_result = space_view
                .contents
                .execute_query(&ctx, &visualizable_entities_per_visualizer);
            let blueprint_query = LatestAtQuery::latest(blueprint_timeline());
            resolver.update_overrides(
                &blueprint,
                &blueprint_query,
                &timeline,
                &space_view_class_registry,
                &legacy_auto_properties,
                &mut query_result,
            );

            // Extract component overrides for testing.
            let mut visited: HashMap<EntityPath, HashMap<ComponentName, EntityPath>> =
                HashMap::default();
            query_result.tree.visit(&mut |node| {
                let result = &node.data_result;
                if let Some(property_overrides) = &result.property_overrides {
                    if !property_overrides.resolved_component_overrides.is_empty() {
                        visited.insert(
                            result.entity_path.clone(),
                            property_overrides
                                .resolved_component_overrides
                                .iter()
                                .map(|(component_name, OverridePath { store_kind, path })| {
                                    assert_eq!(store_kind, &StoreKind::Blueprint);
                                    (*component_name, path.clone())
                                })
                                .collect(),
                        );
                    }
                }
                true
            });

            assert_eq!(visited, expected_overrides, "Scenario {i}");
        }
    }
}
