use nohash_hasher::IntMap;
use slotmap::SlotMap;
use smallvec::SmallVec;

use re_entity_db::{
    external::{re_data_store::LatestAtQuery, re_query::PromiseResult},
    EntityDb, EntityProperties, EntityPropertiesComponent, EntityPropertyMap, EntityTree,
};
use re_log_types::{
    path::RuleEffect, EntityPath, EntityPathFilter, EntityPathRule, EntityPathSubs, Timeline,
};
use re_types::{
    blueprint::{archetypes as blueprint_archetypes, components::QueryExpression},
    Archetype as _, SpaceViewClassIdentifier,
};
use re_types_core::{components::VisualizerOverrides, ComponentName};
use re_viewer_context::{
    DataQueryResult, DataResult, DataResultHandle, DataResultNode, DataResultTree,
    IndicatedEntities, OverridePath, PerVisualizer, PropertyOverrides, QueryRange,
    SpaceViewClassRegistry, SpaceViewId, ViewerContext, VisualizableEntities,
};

use crate::{
    query_view_property, DataQuery, EntityOverrideContext, PropertyResolver, SpaceViewBlueprint,
};

/// An implementation of [`DataQuery`] that is built from a [`blueprint_archetypes::SpaceViewContents`].
///
/// During execution it will walk an [`EntityTree`] and return a [`DataResultTree`]
/// containing any entities that match a [`EntityPathFilter`].
///
/// Note: [`SpaceViewContents`] doesn't implement Clone because it depends on its parent's [`SpaceViewId`]
/// used for identifying the path of its data in the blueprint store. It's ambiguous
/// whether the intent is for a clone to write to the same place.
///
/// If you want a new space view otherwise identical to an existing one, use
/// [`SpaceViewBlueprint::duplicate`].
pub struct SpaceViewContents {
    pub blueprint_entity_path: EntityPath,

    pub space_view_class_identifier: SpaceViewClassIdentifier,
    pub entity_path_filter: EntityPathFilter,
}

impl SpaceViewContents {
    pub fn is_equivalent(&self, other: &SpaceViewContents) -> bool {
        self.space_view_class_identifier
            .eq(&other.space_view_class_identifier)
            && self.entity_path_filter.eq(&other.entity_path_filter)
    }

    /// Checks whether the results of this query "fully contains" the results of another query.
    ///
    /// If this returns `true` then the [`DataQueryResult`] returned by this query should always
    /// contain any [`EntityPath`] that would be included in the results of the other query.
    ///
    /// This is a conservative estimate, and may return `false` in situations where the
    /// query does in fact cover the other query. However, it should never return `true`
    /// in a case where the other query would not be fully covered.
    pub fn entity_path_filter_is_superset_of(&self, other: &SpaceViewContents) -> bool {
        // A query can't fully contain another if their space-view classes don't match
        if self.space_view_class_identifier != other.space_view_class_identifier {
            return false;
        }

        // Anything included by the other query is also included by this query
        self.entity_path_filter
            .is_superset_of(&other.entity_path_filter)
    }
}

impl SpaceViewContents {
    /// Creates a new [`SpaceViewContents`].
    ///
    /// This [`SpaceViewContents`] is ephemeral. It must be saved by calling
    /// `save_to_blueprint_store` on the enclosing `SpaceViewBlueprint`.
    pub fn new(
        id: SpaceViewId,
        space_view_class_identifier: SpaceViewClassIdentifier,
        entity_path_filter: EntityPathFilter,
    ) -> Self {
        // Don't use `entity_path_for_space_view_sub_archetype` here because this will do a search in the future,
        // thus needing the entity tree.
        let blueprint_entity_path = id.as_entity_path().join(&EntityPath::from_single_string(
            blueprint_archetypes::SpaceViewContents::name().short_name(),
        ));

        Self {
            blueprint_entity_path,
            space_view_class_identifier,
            entity_path_filter,
        }
    }

    /// Attempt to load a [`SpaceViewContents`] from the blueprint store.
    pub fn from_db_or_default(
        id: SpaceViewId,
        blueprint_db: &EntityDb,
        query: &LatestAtQuery,
        space_view_class_identifier: SpaceViewClassIdentifier,
        space_env: &EntityPathSubs,
    ) -> Self {
        let (contents, blueprint_entity_path) =
            query_view_property::<blueprint_archetypes::SpaceViewContents>(id, blueprint_db, query);

        let blueprint_archetypes::SpaceViewContents { query } = match contents {
            PromiseResult::Pending => {
                // TODO(#5607): what should happen if the promise is still pending?
                Default::default()
            }
            PromiseResult::Ready(Some(arch)) => arch,
            PromiseResult::Ready(None) => {
                re_log::warn_once!(
                    "Failed to load SpaceViewContents for {:?} from blueprint store at {:?}: not found",
                    id,
                    blueprint_entity_path,
                );
                Default::default()
            }
            PromiseResult::Error(err) => {
                re_log::warn_once!(
                    "Failed to load SpaceViewContents for {:?} from blueprint store at {:?}: {}",
                    id,
                    blueprint_entity_path,
                    err
                );
                Default::default()
            }
        };

        let query = query.iter().map(|qe| qe.0.as_str());

        let entity_path_filter = EntityPathFilter::from_query_expressions(query, space_env);

        Self {
            blueprint_entity_path,
            space_view_class_identifier,
            entity_path_filter,
        }
    }

    /// Persist the entire [`SpaceViewContents`] to the blueprint store.
    ///
    /// This only needs to be called if the [`SpaceViewContents`] was created with [`Self::new`].
    ///
    /// Otherwise, incremental calls to `set_` functions will write just the necessary component
    /// update directly to the store.
    pub fn save_to_blueprint_store(&self, ctx: &ViewerContext<'_>) {
        ctx.save_blueprint_archetype(
            self.blueprint_entity_path.clone(),
            &blueprint_archetypes::SpaceViewContents::new(
                self.entity_path_filter.iter_expressions(),
            ),
        );
    }

    pub fn set_entity_path_filter(
        &self,
        ctx: &ViewerContext<'_>,
        new_entity_path_filter: &EntityPathFilter,
    ) {
        if &self.entity_path_filter == new_entity_path_filter {
            return;
        }

        ctx.save_blueprint_component(
            &self.blueprint_entity_path,
            &new_entity_path_filter
                .iter_expressions()
                .map(|s| QueryExpression(s.into()))
                .collect::<Vec<_>>(),
        );
    }

    pub fn build_resolver<'a>(
        &self,
        space_view_class_registry: &'a re_viewer_context::SpaceViewClassRegistry,
        space_view: &'a SpaceViewBlueprint,
        visualizable_entities_per_visualizer: &'a PerVisualizer<VisualizableEntities>,
        indicated_entities_per_visualizer: &'a PerVisualizer<IndicatedEntities>,
    ) -> DataQueryPropertyResolver<'a> {
        let base_override_root = &self.blueprint_entity_path;
        let individual_override_root =
            base_override_root.join(&DataResult::INDIVIDUAL_OVERRIDES_PREFIX.into());
        let recursive_override_root =
            base_override_root.join(&DataResult::RECURSIVE_OVERRIDES_PREFIX.into());
        DataQueryPropertyResolver {
            space_view_class_registry,
            space_view,
            individual_override_root,
            recursive_override_root,
            visualizable_entities_per_visualizer,
            indicated_entities_per_visualizer,
        }
    }

    /// Remove a subtree and any existing rules that it would match.
    ///
    /// Because most-specific matches win, if we only add a subtree exclusion
    /// it can still be overridden by existing inclusions. This method ensures
    /// that not only do we add a subtree exclusion, but clear out any existing
    /// inclusions or (now redundant) exclusions that would match the subtree.
    pub fn remove_subtree_and_matching_rules(&self, ctx: &ViewerContext<'_>, path: EntityPath) {
        let mut new_entity_path_filter = self.entity_path_filter.clone();
        new_entity_path_filter.remove_subtree_and_matching_rules(path);
        self.set_entity_path_filter(ctx, &new_entity_path_filter);
    }

    /// Directly add an exclusion rule to the [`EntityPathFilter`].
    ///
    /// This is a direct modification of the filter and will not do any simplification
    /// related to overlapping or conflicting rules.
    ///
    /// If you are trying to remove an entire subtree, prefer using [`Self::remove_subtree_and_matching_rules`].
    pub fn raw_add_entity_exclusion(&self, ctx: &ViewerContext<'_>, rule: EntityPathRule) {
        let mut new_entity_path_filter = self.entity_path_filter.clone();
        new_entity_path_filter.add_rule(RuleEffect::Exclude, rule);
        self.set_entity_path_filter(ctx, &new_entity_path_filter);
    }

    /// Directly add an inclusion rule to the [`EntityPathFilter`].
    ///
    /// This is a direct modification of the filter and will not do any simplification
    /// related to overlapping or conflicting rules.
    pub fn raw_add_entity_inclusion(&self, ctx: &ViewerContext<'_>, rule: EntityPathRule) {
        let mut new_entity_path_filter = self.entity_path_filter.clone();
        new_entity_path_filter.add_rule(RuleEffect::Include, rule);
        self.set_entity_path_filter(ctx, &new_entity_path_filter);
    }

    pub fn remove_filter_rule_for(&self, ctx: &ViewerContext<'_>, ent_path: &EntityPath) {
        let mut new_entity_path_filter = self.entity_path_filter.clone();
        new_entity_path_filter.remove_rule_for(ent_path);
        self.set_entity_path_filter(ctx, &new_entity_path_filter);
    }
}

impl DataQuery for SpaceViewContents {
    /// Build up the initial [`DataQueryResult`] for this [`SpaceViewContents`]
    ///
    /// Note that this result will not have any resolved [`PropertyOverrides`]. Those can
    /// be added by separately calling [`PropertyResolver::update_overrides`] on
    /// the result.
    fn execute_query(
        &self,
        ctx: &re_viewer_context::StoreContext<'_>,
        visualizable_entities_for_visualizer_systems: &PerVisualizer<VisualizableEntities>,
    ) -> DataQueryResult {
        re_tracing::profile_function!();

        let mut data_results = SlotMap::<DataResultHandle, DataResultNode>::default();

        let executor =
            QueryExpressionEvaluator::new(self, visualizable_entities_for_visualizer_systems);

        let mut num_matching_entities = 0;
        let mut num_visualized_entities = 0;
        let root_handle = {
            re_tracing::profile_scope!("add_entity_tree_to_data_results_recursive");
            executor.add_entity_tree_to_data_results_recursive(
                ctx.recording.tree(),
                &mut data_results,
                &mut num_matching_entities,
                &mut num_visualized_entities,
            )
        };

        DataQueryResult {
            tree: DataResultTree::new(data_results, root_handle),
            num_matching_entities,
            num_visualized_entities,
        }
    }
}

/// Helper struct for executing the query from [`SpaceViewContents`]
///
/// This restructures the [`QueryExpression`] into several sets that are
/// used to efficiently determine if we should continue the walk or switch
/// to a pure recursive evaluation.
struct QueryExpressionEvaluator<'a> {
    visualizable_entities_for_visualizer_systems: &'a PerVisualizer<VisualizableEntities>,
    entity_path_filter: EntityPathFilter,
}

impl<'a> QueryExpressionEvaluator<'a> {
    fn new(
        blueprint: &'a SpaceViewContents,
        visualizable_entities_for_visualizer_systems: &'a PerVisualizer<VisualizableEntities>,
    ) -> Self {
        re_tracing::profile_function!();

        Self {
            visualizable_entities_for_visualizer_systems,
            entity_path_filter: blueprint.entity_path_filter.clone(),
        }
    }

    fn add_entity_tree_to_data_results_recursive(
        &self,
        tree: &EntityTree,
        data_results: &mut SlotMap<DataResultHandle, DataResultNode>,
        num_matching_entities: &mut usize,
        num_visualized_entities: &mut usize,
    ) -> Option<DataResultHandle> {
        // Early-out optimization
        if !self
            .entity_path_filter
            .is_anything_in_subtree_included(&tree.path)
        {
            return None;
        }

        // TODO(jleibs): If this space is disconnected, we should terminate here

        let entity_path = &tree.path;

        let matches_filter = self.entity_path_filter.is_included(entity_path);
        *num_matching_entities += matches_filter as usize;

        // TODO(#5067): For now, we always start by setting visualizers to the full list of available visualizers.
        // This is currently important for evaluating auto-properties during the space-view `on_frame_start`, which
        // is called before the property-overrider has a chance to update this list.
        // This list will be updated below during `update_overrides_recursive` by calling `choose_default_visualizers`
        // on the space view.
        let visualizers: SmallVec<[_; 4]> = if matches_filter {
            self.visualizable_entities_for_visualizer_systems
                .iter()
                .filter_map(|(visualizer, ents)| ents.contains(entity_path).then_some(*visualizer))
                .collect()
        } else {
            Default::default()
        };
        *num_visualized_entities += !visualizers.is_empty() as usize;

        let children: SmallVec<[_; 4]> = tree
            .children
            .values()
            .filter_map(|subtree| {
                self.add_entity_tree_to_data_results_recursive(
                    subtree,
                    data_results,
                    num_matching_entities,
                    num_visualized_entities,
                )
            })
            .collect();

        // Ignore empty nodes.
        // Since we recurse downwards, this prunes any branches that don't have anything to contribute to the scene
        // and aren't directly included.
        let exact_included = self.entity_path_filter.is_exact_included(entity_path);
        if exact_included || !children.is_empty() || !visualizers.is_empty() {
            Some(data_results.insert(DataResultNode {
                data_result: DataResult {
                    entity_path: entity_path.clone(),
                    visualizers,
                    tree_prefix_only: !matches_filter,
                    property_overrides: None,
                },
                children,
            }))
        } else {
            None
        }
    }
}

pub struct DataQueryPropertyResolver<'a> {
    space_view_class_registry: &'a re_viewer_context::SpaceViewClassRegistry,
    space_view: &'a SpaceViewBlueprint,
    individual_override_root: EntityPath,
    recursive_override_root: EntityPath,
    visualizable_entities_per_visualizer: &'a PerVisualizer<VisualizableEntities>,
    indicated_entities_per_visualizer: &'a PerVisualizer<IndicatedEntities>,
}

impl DataQueryPropertyResolver<'_> {
    /// Helper function to build the [`EntityOverrideContext`] for this [`DataQuery`]
    ///
    /// The context is made up of 3 parts:
    ///  - The root properties are build by merging a stack of paths from the Blueprint Tree. This
    ///  may include properties from the `SpaceView` or `DataQuery`.
    ///  - The individual overrides are found by walking an override subtree under the `data_query/<id>/individual_overrides`
    ///  - The recursive overrides are found by walking an override subtree under the `data_query/<id>/recursive_overrides`
    fn build_override_context<'a>(
        &self,
        blueprint: &EntityDb,
        blueprint_query: &LatestAtQuery,
        active_timeline: &Timeline,
        space_view_class_registry: &SpaceViewClassRegistry,
        legacy_auto_properties: &'a EntityPropertyMap,
    ) -> EntityOverrideContext<'a> {
        re_tracing::profile_function!();

        let legacy_space_view_properties = self
            .space_view
            .legacy_properties(blueprint, blueprint_query);

        let default_query_range = self.space_view.query_range(
            blueprint,
            blueprint_query,
            active_timeline,
            space_view_class_registry,
        );

        // TODO(#4194): Once supported, default entity properties should be passe through here.
        EntityOverrideContext {
            legacy_space_view_properties,
            default_query_range,
            legacy_auto_properties,
        }
    }

    /// Recursively walk the [`DataResultTree`] and update the [`PropertyOverrides`] for each node.
    ///
    /// This will accumulate the recursive properties at each step down the tree, and then merge
    /// with individual overrides on each step.
    #[allow(clippy::too_many_arguments)] // This will be a lot simpler and smaller once `EntityProperties` are gone!
    fn update_overrides_recursive(
        &self,
        blueprint: &EntityDb,
        blueprint_query: &LatestAtQuery,
        active_timeline: &Timeline,
        query_result: &mut DataQueryResult,
        override_context: &EntityOverrideContext<'_>,
        recursive_accumulated_legacy_properties: &EntityProperties,
        recursive_property_overrides: &IntMap<ComponentName, OverridePath>,
        handle: DataResultHandle,
    ) {
        if let Some((
            child_handles,
            recursive_accumulated_legacy_properties,
            recursive_property_overrides,
        )) = query_result.tree.lookup_node_mut(handle).map(|node| {
            let individual_override_path = self
                .individual_override_root
                .join(&node.data_result.entity_path);
            let recursive_override_path = self
                .recursive_override_root
                .join(&node.data_result.entity_path);

            // Special handling for legacy overrides.
            let recursive_legacy_properties = blueprint
                .latest_at_component_quiet::<EntityPropertiesComponent>(
                    &recursive_override_path,
                    blueprint_query,
                )
                .map(|result| result.value.0);
            let individual_legacy_properties = blueprint
                .latest_at_component_quiet::<EntityPropertiesComponent>(
                    &individual_override_path,
                    blueprint_query,
                )
                .map(|result| result.value.0);

            let recursive_accumulated_legacy_properties =
                if let Some(recursive_legacy_properties) = recursive_legacy_properties.as_ref() {
                    recursive_accumulated_legacy_properties.with_child(recursive_legacy_properties)
                } else {
                    recursive_accumulated_legacy_properties.clone()
                };
            let default_legacy_properties = override_context
                .legacy_auto_properties
                .get(&node.data_result.entity_path);
            let accumulated_legacy_properties =
                if let Some(individual) = individual_legacy_properties.as_ref() {
                    recursive_accumulated_legacy_properties
                        .with_child(individual)
                        .with_child(&default_legacy_properties)
                } else {
                    recursive_accumulated_legacy_properties.with_child(&default_legacy_properties)
                };

            // Update visualizers from overrides.
            if !node.data_result.visualizers.is_empty() {
                re_tracing::profile_scope!("Update visualizers from overrides");

                // If the user has overridden the visualizers, update which visualizers are used.
                // TODO(#5607): what should happen if the promise is still pending?
                if let Some(viz_override) = blueprint
                    .latest_at_component::<VisualizerOverrides>(
                        &individual_override_path,
                        blueprint_query,
                    )
                    .map(|c| c.value)
                {
                    node.data_result.visualizers =
                        viz_override.0.iter().map(|v| v.as_str().into()).collect();
                } else {
                    // Otherwise ask the `SpaceViewClass` to choose.
                    node.data_result.visualizers = self
                        .space_view
                        .class(self.space_view_class_registry)
                        .choose_default_visualizers(
                            &node.data_result.entity_path,
                            self.visualizable_entities_per_visualizer,
                            self.indicated_entities_per_visualizer,
                        );
                }
            }

            // First, gather recursive overrides. Previous recursive overrides are the base for the next.
            // We assume that most of the time there's no new recursive overrides, so clone the map lazily.
            let mut recursive_property_overrides =
                std::borrow::Cow::Borrowed(recursive_property_overrides);
            if let Some(recursive_override_subtree) =
                blueprint.tree().subtree(&recursive_override_path)
            {
                for component in recursive_override_subtree.entity.components.keys() {
                    if let Some(component_data) = blueprint
                        .store()
                        .latest_at(
                            blueprint_query,
                            &recursive_override_path,
                            *component,
                            &[*component],
                        )
                        .and_then(|(_, _, cells)| cells[0].clone())
                    {
                        if !component_data.is_empty() {
                            recursive_property_overrides.to_mut().insert(
                                *component,
                                OverridePath::blueprint_path(recursive_override_path.clone()),
                            );
                        }
                    }
                }
            }

            // Then, gather individual overrides - these may override the recursive ones again,
            // but recursive overrides are still inherited to children.
            let mut resolved_component_overrides = (*recursive_property_overrides).clone();
            if let Some(individual_override_subtree) =
                blueprint.tree().subtree(&individual_override_path)
            {
                for component in individual_override_subtree.entity.components.keys() {
                    if let Some(component_data) = blueprint
                        .store()
                        .latest_at(
                            blueprint_query,
                            &individual_override_path,
                            *component,
                            &[*component],
                        )
                        .and_then(|(_, _, cells)| cells[0].clone())
                    {
                        if !component_data.is_empty() {
                            resolved_component_overrides.insert(
                                *component,
                                OverridePath::blueprint_path(individual_override_path.clone()),
                            );
                        }
                    }
                }
            }

            // Figure out relevant visual time range.
            let visible_time_range_archetype = blueprint
                .latest_at_archetype::<blueprint_archetypes::VisibleTimeRanges>(
                    &recursive_override_path,
                    blueprint_query,
                )
                .ok()
                .flatten();
            let time_range = visible_time_range_archetype
                .as_ref()
                .and_then(|(_, arch)| arch.range_for_timeline(active_timeline.name().as_str()));
            let query_range = time_range.map_or_else(
                || override_context.default_query_range.clone(),
                |time_range| QueryRange::TimeRange(time_range.clone()),
            );

            node.data_result.property_overrides = Some(PropertyOverrides {
                accumulated_properties: accumulated_legacy_properties,
                individual_properties: individual_legacy_properties,
                recursive_properties: recursive_legacy_properties,
                resolved_component_overrides,
                recursive_override_path,
                individual_override_path,
                query_range,
            });

            (
                node.children.clone(),
                recursive_accumulated_legacy_properties,
                recursive_property_overrides,
            )
        }) {
            for child in child_handles {
                self.update_overrides_recursive(
                    blueprint,
                    blueprint_query,
                    active_timeline,
                    query_result,
                    override_context,
                    &recursive_accumulated_legacy_properties,
                    &recursive_property_overrides,
                    child,
                );
            }
        }
    }
}

impl<'a> PropertyResolver for DataQueryPropertyResolver<'a> {
    /// Recursively walk the [`DataResultTree`] and update the [`PropertyOverrides`] for each node.
    fn update_overrides(
        &self,
        blueprint: &EntityDb,
        blueprint_query: &LatestAtQuery,
        active_timeline: &Timeline,
        space_view_class_registry: &SpaceViewClassRegistry,
        legacy_auto_properties: &EntityPropertyMap,
        query_result: &mut DataQueryResult,
    ) {
        re_tracing::profile_function!();
        let override_context = self.build_override_context(
            blueprint,
            blueprint_query,
            active_timeline,
            space_view_class_registry,
            legacy_auto_properties,
        );

        if let Some(root) = query_result.tree.root_handle() {
            let accumulated_legacy_properties = EntityProperties::default();
            let recursive_property_overrides = Default::default();

            self.update_overrides_recursive(
                blueprint,
                blueprint_query,
                active_timeline,
                query_result,
                &override_context,
                &accumulated_legacy_properties,
                &recursive_property_overrides,
                root,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use re_entity_db::EntityDb;
    use re_log_types::{example_components::MyPoint, DataRow, RowId, StoreId, TimePoint, Timeline};
    use re_viewer_context::{StoreContext, StoreHub, VisualizableEntities};

    use super::*;

    #[test]
    fn test_query_results() {
        let space_env = Default::default();

        let mut recording = EntityDb::new(StoreId::random(re_log_types::StoreKind::Recording));
        let blueprint = EntityDb::new(StoreId::random(re_log_types::StoreKind::Blueprint));

        let timeline_frame = Timeline::new_sequence("frame");
        let timepoint = TimePoint::from_iter([(timeline_frame, 10)]);

        // Set up a store DB with some entities
        for entity_path in ["parent", "parent/skipped/child1", "parent/skipped/child2"] {
            let row_id = RowId::new();
            let point = MyPoint::new(1.0, 2.0);
            let row = DataRow::from_component_batches(
                row_id,
                timepoint.clone(),
                entity_path.into(),
                [&[point] as _],
            )
            .unwrap();

            recording.add_data_row(row).unwrap();
        }

        let mut visualizable_entities_for_visualizer_systems =
            PerVisualizer::<VisualizableEntities>::default();

        visualizable_entities_for_visualizer_systems
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

        let ctx = StoreContext {
            app_id: re_log_types::ApplicationId::unknown(),
            blueprint: &blueprint,
            default_blueprint: None,
            recording: &recording,
            bundle: &Default::default(),
            hub: &StoreHub::test_hub(),
        };

        struct Scenario {
            filter: &'static str,
            outputs: Vec<&'static str>,
        }

        let scenarios: Vec<Scenario> = vec![
            Scenario {
                filter: "+ /**",
                outputs: vec![
                    "/**",
                    "/parent",
                    "/parent/skipped",
                    "/parent/skipped/child1", // Only child 1 has visualizers
                ],
            },
            Scenario {
                filter: "+ parent/skipped/**",
                outputs: vec![
                    "/**",
                    "/parent/**", // Only included because is a prefix
                    "/parent/skipped",
                    "/parent/skipped/child1", // Only child 1 has visualizers
                ],
            },
            Scenario {
                filter: r"+ parent
                          + parent/skipped/child2",
                outputs: vec![
                    "/**", // Trivial intermediate group -- could be collapsed
                    "/parent",
                    "/parent/skipped/**", // Trivial intermediate group -- could be collapsed
                    "/parent/skipped/child2",
                ],
            },
            Scenario {
                filter: r"+ parent/skipped
                          + parent/skipped/child2
                          + parent/**",
                outputs: vec![
                    "/**",
                    "/parent",
                    "/parent/skipped",        // Included because an exact match
                    "/parent/skipped/child1", // Included because an exact match
                    "/parent/skipped/child2",
                ],
            },
            Scenario {
                filter: r"+ parent/skipped
                          + parent/skipped/child2
                          + parent/**
                          - parent",
                outputs: vec![
                    "/**",
                    "/parent/**",             // Parent leaf has been excluded
                    "/parent/skipped",        // Included because an exact match
                    "/parent/skipped/child1", // Included because an exact match
                    "/parent/skipped/child2",
                ],
            },
            Scenario {
                filter: r"+ parent/**
                          - parent/skipped/**",
                outputs: vec!["/**", "/parent"], // None of the children are hit since excluded
            },
            Scenario {
                filter: r"+ parent/**
                          + parent/skipped/child2
                          - parent/skipped/child1",
                outputs: vec![
                    "/**",
                    "/parent",
                    "/parent/skipped",
                    "/parent/skipped/child2", // No child1 since skipped.
                ],
            },
            Scenario {
                filter: r"+ not/found",
                // TODO(jleibs): Making this work requires merging the EntityTree walk with a minimal-coverage ExactMatchTree walk
                // not crucial for now until we expose a free-form UI for entering paths.
                // vec!["/**", "not/**", "not/found"]),
                outputs: vec![],
            },
        ];

        for (i, Scenario { filter, outputs }) in scenarios.into_iter().enumerate() {
            let contents = SpaceViewContents::new(
                SpaceViewId::random(),
                "3D".into(),
                EntityPathFilter::parse_forgiving(filter, &space_env),
            );

            let query_result =
                contents.execute_query(&ctx, &visualizable_entities_for_visualizer_systems);

            let mut visited = vec![];
            query_result.tree.visit(&mut |node| {
                let result = &node.data_result;
                if result.entity_path == EntityPath::root() {
                    visited.push("/**".to_owned());
                } else if result.tree_prefix_only {
                    visited.push(format!("{}/**", result.entity_path));
                    assert!(result.visualizers.is_empty());
                } else {
                    visited.push(result.entity_path.to_string());
                }
                true
            });

            assert_eq!(visited, outputs, "Scenario {i}, filter: {filter}");
        }
    }
}
