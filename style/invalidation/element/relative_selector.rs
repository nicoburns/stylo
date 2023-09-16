/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Invalidation of element styles relative selectors.

use crate::data::ElementData;
use crate::dom::{TElement, TNode};
use crate::invalidation::element::invalidation_map::{
    Dependency, DependencyInvalidationKind, RelativeDependencyInvalidationKind, NormalDependencyInvalidationKind, RelativeSelectorInvalidationMap,
};
use crate::invalidation::element::invalidator::{
    DescendantInvalidationLists, Invalidation, InvalidationProcessor, InvalidationResult,
    InvalidationVector, SiblingTraversalMap, TreeStyleInvalidator,
};
use crate::invalidation::element::restyle_hints::RestyleHint;
use crate::invalidation::element::state_and_attributes::{
    dependency_may_be_relevant, invalidated_descendants, invalidated_self, invalidated_sibling,
    push_invalidation, should_process_descendants,
};
use crate::stylist::{CascadeData, Stylist};
use dom::ElementState;
use fxhash::FxHashMap;
use selectors::matching::{
    ElementSelectorFlags, MatchingContext, MatchingForInvalidation, MatchingMode,
    NeedsSelectorFlags, QuirksMode, SelectorCaches, VisitedHandlingMode,
};
use selectors::OpaqueElement;
use selectors::parser::SelectorKey;
use smallvec::SmallVec;
use std::ops::DerefMut;

/// Overall invalidator for handling relative selector invalidations.
pub struct RelativeSelectorInvalidator<'a, E>
where
    E: TElement + 'a,
{
    /// Element triggering the invalidation.
    pub element: E,
    /// Quirks mode of the current invalidation.
    pub quirks_mode: QuirksMode,
    /// Callback to trigger when the subject element is invalidated.
    pub invalidated: fn(E, &InvalidationResult),
    /// The traversal map that should be used to process invalidations.
    pub sibling_traversal_map: SiblingTraversalMap<E>,
    /// Marker for 'a lifetime.
    pub _marker: ::std::marker::PhantomData<&'a ()>,
}

struct RelativeSelectorInvalidation<'a, E>
where
    E: TElement + 'a,
{
    host: Option<E>,
    kind: RelativeDependencyInvalidationKind,
    dependency: &'a Dependency,
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct InvalidationKey(SelectorKey, DependencyInvalidationKind);

/// Interface for collecting relative selector dependencies.
pub struct RelativeSelectorDependencyCollector<'a, E>
where
    E: TElement,
{
    /// Maps an invalidation into its scope, selector offset, and its outer dependency.
    invalidations: FxHashMap<InvalidationKey, (Option<E>, usize, &'a Dependency)>,
    /// The top element in the subtree being invalidated.
    top: E,
}

type Invalidations<'a, E> = SmallVec<[RelativeSelectorInvalidation<'a, E>; 1]>;

struct ToInvalidate<'a, E: TElement + 'a> {
    // Dependencies already invalidated.
    invalidations: Invalidations<'a, E>,
}

impl<'a, E: TElement + 'a> Default for ToInvalidate<'a, E> {
    fn default() -> Self {
        Self {
            invalidations: Invalidations::default(),
        }
    }
}

impl<'a, E> RelativeSelectorDependencyCollector<'a, E>
where
    E: TElement,
{
    fn new(top: E) -> Self {
        Self {
            invalidations: FxHashMap::default(),
            top,
        }
    }

    fn insert_invalidation(
        &mut self,
        key: InvalidationKey,
        offset: usize,
        outer: &'a Dependency,
        host: Option<E>,
    ) {
        self.invalidations
            .entry(key)
            .and_modify(|(h, o, d)| {
                // Just keep one.
                if *o <= offset {
                    return;
                }
                (*h, *o, *d) = (host, offset, outer);
            })
            .or_insert_with(|| (host, offset, outer));
    }

    /// Add this dependency, if it is unique (i.e. Different outer dependency or same outer dependency
    /// but requires a different invalidation traversal).
    pub fn add_dependency(
        &mut self,
        dependency: &'a Dependency,
        element: E,
        host: Option<E>,
    ) {
        match dependency.invalidation_kind() {
            DependencyInvalidationKind::Normal(..) => unreachable!("TODO inner selector"),
            DependencyInvalidationKind::Relative(kind) => {
                debug_assert!(dependency.parent.is_some(), "Orphaned inner relative selector?");
                if element.relative_selector_search_direction().is_none() {
                    return;
                }
                if element != self.top && matches!(kind, RelativeDependencyInvalidationKind::Parent |
                    RelativeDependencyInvalidationKind::PrevSibling |
                    RelativeDependencyInvalidationKind::EarlierSibling)
                {
                    return;
                }
                self.insert_invalidation(
                    InvalidationKey(SelectorKey::new(&dependency.selector), dependency.invalidation_kind()),
                    dependency.selector_offset,
                    dependency.parent.as_ref().unwrap(),
                    host);
                // We move the invalidation up to the top of the subtree to avoid unnecessary traveral, but
                // this means that we need to take ancestor-earlier sibling invalidations into account, as
                // they'd look into earlier siblings of the top of the subtree as well.
                if element != self.top && matches!(kind, RelativeDependencyInvalidationKind::AncestorEarlierSibling |
                    RelativeDependencyInvalidationKind::AncestorPrevSibling)
                {
                    self.insert_invalidation(
                        InvalidationKey(
                            SelectorKey::new(&dependency.selector),
                            if matches!(kind, RelativeDependencyInvalidationKind::AncestorPrevSibling) {
                                DependencyInvalidationKind::Relative(RelativeDependencyInvalidationKind::PrevSibling)
                            } else {
                                DependencyInvalidationKind::Relative(RelativeDependencyInvalidationKind::EarlierSibling)
                            }
                        ),
                        dependency.selector_offset,
                        dependency.parent.as_ref().unwrap(),
                        host);
                }
            },
        };
    }

    /// Get the dependencies in a list format.
    fn get(self) -> ToInvalidate<'a, E> {
        let mut result = ToInvalidate::default();
        for (key, (host, _offset, dependency)) in self.invalidations {
            match key.1 {
                DependencyInvalidationKind::Normal(_) => unreachable!("TODO inner selector"),
                DependencyInvalidationKind::Relative(kind) =>
                    result.invalidations.push(RelativeSelectorInvalidation { kind, host, dependency }),
            };
        }
        result
    }

    fn collect_all_dependencies_for_element(
        &mut self,
        element: E,
        scope: &Option<E>,
        quirks_mode: QuirksMode,
        map: &'a RelativeSelectorInvalidationMap,
        accept: fn(&Dependency) -> bool,
    ) {
        element
            .id()
            .map(|v| match map.map.id_to_selector.get(v, quirks_mode) {
                Some(v) => {
                    for dependency in v {
                        if !accept(dependency) {
                            continue;
                        }
                        self.add_dependency(dependency, element, *scope);
                    }
                },
                None => (),
            });
        element.each_class(|v| match map.map.class_to_selector.get(v, quirks_mode) {
            Some(v) => {
                for dependency in v {
                    if !accept(dependency) {
                        continue;
                    }
                    self.add_dependency(dependency, element, *scope);
                }
            },
            None => (),
        });
        element.each_attr_name(
            |v| match map.map.other_attribute_affecting_selectors.get(v) {
                Some(v) => {
                    for dependency in v {
                        if !accept(dependency) {
                            continue;
                        }
                        self.add_dependency(dependency, element, *scope);
                    }
                },
                None => (),
            },
        );
        let state = element.state();
        map.map.state_affecting_selectors.lookup_with_additional(
            element,
            quirks_mode,
            None,
            &[],
            ElementState::empty(),
            |dependency| {
                if !dependency.state.intersects(state) {
                    return true;
                }
                if !accept(&dependency.dep) {
                    return true;
                }
                self.add_dependency(&dependency.dep, element, *scope);
                true
            },
        );

        if let Some(v) = map.type_to_selector.get(element.local_name()) {
            for dependency in v {
                if !accept(dependency) {
                    continue;
                }
                self.add_dependency(dependency, element, *scope);
            }
        }

        for dependency in &map.any_to_selector {
            if !accept(dependency) {
                continue;
            }
            self.add_dependency(dependency, element, *scope);
        }
    }
}

impl<'a, E> RelativeSelectorInvalidator<'a, E>
where
    E: TElement + 'a,
{
    /// Gather relative selector dependencies for the given element, and invalidate as necessary.
    pub fn invalidate_relative_selectors_for_this<F>(
        self,
        stylist: &'a Stylist,
        gather_dependencies: F,
    ) where
        F: Fn(
            &E,
            &Option<E>,
            &'a CascadeData,
            QuirksMode,
            &mut RelativeSelectorDependencyCollector<'a, E>,
        ),
    {
        let mut collector = RelativeSelectorDependencyCollector::new(self.element);
        stylist.for_each_cascade_data_with_scope(self.element, |data, scope| {
            let map = data.relative_selector_invalidation_map();
            if !map.used {
                return;
            }
            gather_dependencies(
                &self.element,
                &scope,
                data,
                self.quirks_mode,
                &mut collector,
            );
        });
        self.invalidate_from_dependencies(collector.get());
    }

    /// Gather relative selector dependencies for the given element (And its subtree) that mutated, and invalidate as necessary.
    pub fn invalidate_relative_selectors_for_dom_mutation(
        self,
        subtree: bool,
        stylist: &'a Stylist,
        inherited_search_path: ElementSelectorFlags,
        accept: fn(&Dependency) -> bool,
    ) {
        let mut collector = RelativeSelectorDependencyCollector::<'a, E>::new(self.element);
        let mut traverse_subtree = false;
        self.element.apply_selector_flags(inherited_search_path);
        stylist.for_each_cascade_data_with_scope(self.element, |data, scope| {
            let map = data.relative_selector_invalidation_map();
            if !map.used {
                return;
            }
            traverse_subtree |= map.needs_ancestors_traversal;
            collector.collect_all_dependencies_for_element(self.element, &scope, self.quirks_mode, map, accept);
        });

        if subtree && traverse_subtree {
            for node in self.element.as_node().dom_descendants() {
                let descendant = match node.as_element() {
                    Some(e) => e,
                    None => continue,
                };
                descendant.apply_selector_flags(inherited_search_path);
                stylist.for_each_cascade_data_with_scope(descendant, |data, scope| {
                    let map = data.relative_selector_invalidation_map();
                    if !map.used {
                        return;
                    }
                    collector.collect_all_dependencies_for_element(descendant, &scope, self.quirks_mode, map, accept);
                });
            }
        }
        self.invalidate_from_dependencies(collector.get());
    }

    /// Carry out complete invalidation triggered by a relative selector invalidation.
    /// Updates the relative selector search path if provided.
    fn invalidate_from_dependencies(&self, to_invalidate: ToInvalidate<'a, E>) {
        for invalidation in to_invalidate.invalidations {
            self.invalidate_upwards(&invalidation);
        }
    }

    fn invalidate_upwards(&self, invalidation: &RelativeSelectorInvalidation<'a, E>) {
        let host = invalidation.host.map(|e| e.opaque());
        let outer_dependency = invalidation.dependency;
        // This contains the main reason for why relative selector invalidation is handled
        // separately - It travels ancestor and/or earlier sibling direction.
        match invalidation.kind {
            RelativeDependencyInvalidationKind::Parent => {
                self.element.parent_element().map(|e| {
                    if !Self::in_search_direction(
                        &e,
                        ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR,
                    ) {
                        return;
                    }
                    self.handle_anchor(e, outer_dependency, host);
                });
            },
            RelativeDependencyInvalidationKind::Ancestors => {
                let mut parent = self.element.parent_element();
                while let Some(par) = parent {
                    if !Self::in_search_direction(
                        &par,
                        ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR,
                    ) {
                        return;
                    }
                    self.handle_anchor(par, outer_dependency, host);
                    parent = par.parent_element();
                }
            },
            RelativeDependencyInvalidationKind::PrevSibling => {
                self.sibling_traversal_map
                    .prev_sibling_for(&self.element)
                    .map(|e| {
                        if !Self::in_search_direction(
                            &e,
                            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                        ) {
                            return;
                        }
                        self.handle_anchor(e, outer_dependency, host);
                    });
            },
            RelativeDependencyInvalidationKind::AncestorPrevSibling => {
                let mut parent = self.element.parent_element();
                while let Some(par) = parent {
                    if !Self::in_search_direction(
                        &par,
                        ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR,
                    ) {
                        return;
                    }
                    par.prev_sibling_element().map(|e| {
                        if !Self::in_search_direction(
                            &e,
                            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                        ) {
                            return;
                        }
                        self.handle_anchor(e, outer_dependency, host);
                    });
                    parent = par.parent_element();
                }
            },
            RelativeDependencyInvalidationKind::EarlierSibling => {
                let mut sibling = self.sibling_traversal_map.prev_sibling_for(&self.element);
                while let Some(sib) = sibling {
                    if !Self::in_search_direction(
                        &sib,
                        ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                    ) {
                        return;
                    }
                    self.handle_anchor(sib, outer_dependency, host);
                    sibling = sib.prev_sibling_element();
                }
            },
            RelativeDependencyInvalidationKind::AncestorEarlierSibling => {
                let mut parent = self.element.parent_element();
                while let Some(par) = parent {
                    if !Self::in_search_direction(
                        &par,
                        ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR,
                    ) {
                        return;
                    }
                    let mut sibling = par.prev_sibling_element();
                    while let Some(sib) = sibling {
                        if !Self::in_search_direction(
                            &sib,
                            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                        ) {
                            return;
                        }
                        self.handle_anchor(sib, outer_dependency, host);
                        sibling = sib.prev_sibling_element();
                    }
                    parent = par.parent_element();
                }
            },
        }
    }

    /// Is this element in the direction of the given relative selector search path?
    fn in_search_direction(element: &E, desired: ElementSelectorFlags) -> bool {
        if let Some(direction) = element.relative_selector_search_direction() {
            direction.intersects(desired)
        } else {
            false
        }
    }

    /// Handle a potential relative selector anchor.
    fn handle_anchor(
        &self,
        element: E,
        outer_dependency: &Dependency,
        host: Option<OpaqueElement>,
    ) {
        let is_rightmost = Self::is_subject(outer_dependency);
        if (is_rightmost && !element.has_selector_flags(ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR)) ||
            (!is_rightmost && !element.has_selector_flags(ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR_NON_SUBJECT))
        {
            // If it was never a relative selector anchor, don't bother.
            return;
        }
        let mut selector_caches = SelectorCaches::default();
        let matching_context = MatchingContext::<'_, E::Impl>::new_for_visited(
            MatchingMode::Normal,
            None,
            &mut selector_caches,
            VisitedHandlingMode::AllLinksVisitedAndUnvisited,
            self.quirks_mode,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::Yes,
        );
        let mut data = match element.mutate_data() {
            Some(data) => data,
            None => return,
        };
        let mut processor = RelativeSelectorOuterInvalidationProcessor {
            element,
            host,
            data: data.deref_mut(),
            dependency: &*outer_dependency,
            matching_context,
            traversal_map: SiblingTraversalMap::default(),
        };
        let result = TreeStyleInvalidator::new(element, None, &mut processor).invalidate();
        (self.invalidated)(element, &result);
    }

    /// Does this relative selector dependency have its relative selector in the subject position?
    fn is_subject(outer_dependency: &Dependency) -> bool {
        debug_assert!(
            matches!(outer_dependency.invalidation_kind(), DependencyInvalidationKind::Normal(_)),
            "Outer selector of relative selector is relative?");
        match outer_dependency.parent {
            Some(ref p) => Self::is_subject(p.as_ref()),
            None => outer_dependency.selector_offset == 0,
        }
    }

}

/// Blindly invalidate everything outside of a relative selector.
/// Consider `:is(.a :has(.b) .c ~ .d) ~ .e .f`, where .b gets deleted.
/// Since the tree mutated, we cannot rely on snapshots.
pub struct RelativeSelectorOuterInvalidationProcessor<'a, 'b, E: TElement> {
    /// Element being invalidated.
    pub element: E,
    /// The current shadow host, if any.
    pub host: Option<OpaqueElement>,
    /// Data for the element being invalidated.
    pub data: &'a mut ElementData,
    /// Dependency to be processed.
    pub dependency: &'b Dependency,
    /// Matching context to use for invalidation.
    pub matching_context: MatchingContext<'a, E::Impl>,
    /// Traversal map for this invalidation.
    pub traversal_map: SiblingTraversalMap<E>,
}


impl<'a, 'b: 'a, E: 'a> InvalidationProcessor<'b, 'a, E>
    for RelativeSelectorOuterInvalidationProcessor<'a, 'b, E>
where
    E: TElement,
{
    fn invalidates_on_pseudo_element(&self) -> bool {
        true
    }

    fn check_outer_dependency(&mut self, _dependency: &Dependency, _element: E) -> bool {
        // At this point, we know a relative selector invalidated, and are ignoring them.
        true
    }

    fn matching_context(&mut self) -> &mut MatchingContext<'a, E::Impl> {
        &mut self.matching_context
    }

    fn sibling_traversal_map(&self) -> &SiblingTraversalMap<E> {
        &self.traversal_map
    }

    fn collect_invalidations(
        &mut self,
        element: E,
        _self_invalidations: &mut InvalidationVector<'b>,
        descendant_invalidations: &mut DescendantInvalidationLists<'b>,
        sibling_invalidations: &mut InvalidationVector<'b>,
    ) -> bool {
        debug_assert_eq!(element, self.element);
        debug_assert!(
            self.matching_context
                .matching_for_invalidation(),
            "Not matching for invalidation?"
        );

        let invalidated_self = add_blind_invalidation(
            &element,
            self.dependency,
            &self.host,
            descendant_invalidations,
            sibling_invalidations,
        );
        if invalidated_self {
            self.data.hint.insert(RestyleHint::RESTYLE_SELF);
        }
        invalidated_self
    }

    fn should_process_descendants(&mut self, element: E) -> bool {
        if element == self.element {
            return should_process_descendants(&self.data);
        }

        match element.borrow_data() {
            Some(d) => should_process_descendants(&d),
            None => return false,
        }
    }

    fn recursion_limit_exceeded(&mut self, _element: E) {
        unreachable!("Unexpected recursion limit");
    }

    fn invalidated_descendants(&mut self, element: E, child: E) {
        invalidated_descendants(element, child)
    }

    fn invalidated_self(&mut self, element: E) {
        debug_assert_ne!(element, self.element);
        invalidated_self(element);
    }

    fn invalidated_sibling(&mut self, element: E, of: E) {
        debug_assert_ne!(element, self.element);
        invalidated_sibling(element, of);
    }
}

/// Just add this dependency as invalidation without running any selector check.
fn add_blind_invalidation<'a, E: TElement>(
    element: &E,
    dependency: &'a Dependency,
    current_host: &Option<OpaqueElement>,
    descendant_invalidations: &mut DescendantInvalidationLists<'a>,
    sibling_invalidations: &mut InvalidationVector<'a>,
) -> bool {
    debug_assert!(
        matches!(dependency.invalidation_kind(), DependencyInvalidationKind::Normal(_)),
        "Unexpected relative dependency"
    );
    if !dependency_may_be_relevant(dependency, element, false) {
        return false;
    }
    let invalidation_kind = dependency.normal_invalidation_kind();
    if matches!(invalidation_kind, NormalDependencyInvalidationKind::Element) {
        if let Some(ref parent) = dependency.parent {
            return add_blind_invalidation(
                element,
                parent,
                current_host,
                descendant_invalidations,
                sibling_invalidations,
            );
        }
        return true;
    }

    debug_assert_ne!(dependency.selector_offset, 0);
    debug_assert_ne!(dependency.selector_offset, dependency.selector.len());

    let invalidation = Invalidation::new(&dependency, *current_host);

    push_invalidation(
        invalidation,
        invalidation_kind,
        descendant_invalidations,
        sibling_invalidations,
    )
}