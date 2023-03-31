// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::iter::Peekable;
use std::sync::Arc;

use itertools::Itertools;

use crate::backend::{ChangeId, CommitId, MillisSinceEpoch, ObjectId};
use crate::default_index_store::{CompositeIndex, IndexEntry, IndexEntryByPosition, IndexPosition};
use crate::default_revset_graph_iterator::RevsetGraphIterator;
use crate::index::{HexPrefix, Index, PrefixResolution};
use crate::matchers::{EverythingMatcher, Matcher, PrefixMatcher};
use crate::revset::{
    ChangeIdIndex, Revset, RevsetError, RevsetExpression, RevsetFilterPredicate, RevsetGraphEdge,
    GENERATION_RANGE_FULL,
};
use crate::store::Store;
use crate::{backend, rewrite};

#[derive(Debug, Clone, Copy, PartialEq)]
enum PredicateMatch {
    /// The predicate matches the entry.
    Match,
    /// The predicate does not match the entry, but may match later entries
    /// (with lower IndexPosition).
    NotThisOne,
    /// The predicate does not match the entry, and will not match any later
    /// entries.
    NeverAgain,
}

impl PredicateMatch {
    fn from_boolean(b: bool) -> Self {
        if b {
            PredicateMatch::Match
        } else {
            PredicateMatch::NotThisOne
        }
    }

    fn and(self, other: PredicateMatch) -> PredicateMatch {
        match (self, other) {
            (PredicateMatch::Match, PredicateMatch::Match) => PredicateMatch::Match,
            (PredicateMatch::NeverAgain, _) => PredicateMatch::NeverAgain,
            (_, PredicateMatch::NeverAgain) => PredicateMatch::NeverAgain,
            _ => PredicateMatch::NotThisOne,
        }
    }

    fn or(self, other: PredicateMatch) -> PredicateMatch {
        match (self, other) {
            (PredicateMatch::NeverAgain, PredicateMatch::NeverAgain) => PredicateMatch::NeverAgain,
            (PredicateMatch::Match, _) => PredicateMatch::Match,
            (_, PredicateMatch::Match) => PredicateMatch::Match,
            _ => PredicateMatch::NotThisOne,
        }
    }

    fn and_not(self, other: PredicateMatch) -> PredicateMatch {
        match (self, other) {
            (PredicateMatch::NeverAgain, _) => PredicateMatch::NeverAgain,
            (_, PredicateMatch::Match) => PredicateMatch::NotThisOne,
            (PredicateMatch::Match, _) => PredicateMatch::Match,
            _ => PredicateMatch::NotThisOne,
        }
    }
}

trait ToPredicateFn<'index> {
    /// Creates function that tests if the given entry is included in the set.
    ///
    /// The predicate function is evaluated in order of `RevsetIterator`.
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_>;
}

impl<'index, T> ToPredicateFn<'index> for Box<T>
where
    T: ToPredicateFn<'index> + ?Sized,
{
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        <T as ToPredicateFn<'index>>::to_predicate_fn(self)
    }
}

trait InternalRevset<'index>: ToPredicateFn<'index> {
    // All revsets currently iterate in order of descending index position
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_>;
}

pub struct RevsetImpl<'index> {
    inner: Box<dyn InternalRevset<'index> + 'index>,
    index: CompositeIndex<'index>,
}

impl<'index> RevsetImpl<'index> {
    fn new(
        revset: Box<dyn InternalRevset<'index> + 'index>,
        index: CompositeIndex<'index>,
    ) -> Self {
        Self {
            inner: revset,
            index,
        }
    }

    pub fn iter_graph_impl(&self) -> RevsetGraphIterator<'_, 'index> {
        RevsetGraphIterator::new(self.inner.iter())
    }
}

impl<'index> Revset<'index> for RevsetImpl<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = CommitId> + '_> {
        Box::new(self.inner.iter().map(|index_entry| index_entry.commit_id()))
    }

    fn iter_graph(&self) -> Box<dyn Iterator<Item = (CommitId, Vec<RevsetGraphEdge>)> + '_> {
        Box::new(RevsetGraphIterator::new(self.inner.iter()))
    }

    fn change_id_index(&self) -> Box<dyn ChangeIdIndex + 'index> {
        // TODO: Create a persistent lookup from change id to commit ids.
        let mut pos_by_change = vec![];
        for entry in self.inner.iter() {
            pos_by_change.push((entry.change_id(), entry.position()));
        }
        let pos_by_change = IdIndex::from_vec(pos_by_change);
        Box::new(ChangeIdIndexImpl {
            index: self.index.clone(),
            pos_by_change,
        })
    }

    fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

struct ChangeIdIndexImpl<'index> {
    index: CompositeIndex<'index>,
    pos_by_change: IdIndex<ChangeId, IndexPosition>,
}

impl ChangeIdIndex for ChangeIdIndexImpl<'_> {
    fn resolve_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<CommitId>> {
        self.pos_by_change
            .resolve_prefix_with(prefix, |pos| self.index.entry_by_pos(*pos).commit_id())
    }

    fn shortest_unique_prefix_len(&self, change_id: &ChangeId) -> usize {
        self.pos_by_change.shortest_unique_prefix_len(change_id)
    }
}

#[derive(Debug, Clone)]
struct IdIndex<K, V>(Vec<(K, V)>);

impl<K, V> IdIndex<K, V>
where
    K: ObjectId + Ord,
{
    /// Creates new index from the given entries. Multiple values can be
    /// associated with a single key.
    pub fn from_vec(mut vec: Vec<(K, V)>) -> Self {
        vec.sort_unstable_by(|(k0, _), (k1, _)| k0.cmp(k1));
        IdIndex(vec)
    }

    /// Looks up entries with the given prefix, and collects values if matched
    /// entries have unambiguous keys.
    pub fn resolve_prefix_with<U>(
        &self,
        prefix: &HexPrefix,
        mut value_mapper: impl FnMut(&V) -> U,
    ) -> PrefixResolution<Vec<U>> {
        let mut range = self.resolve_prefix_range(prefix).peekable();
        if let Some((first_key, _)) = range.peek().copied() {
            let maybe_entries: Option<Vec<_>> = range
                .map(|(k, v)| (k == first_key).then(|| value_mapper(v)))
                .collect();
            if let Some(entries) = maybe_entries {
                PrefixResolution::SingleMatch(entries)
            } else {
                PrefixResolution::AmbiguousMatch
            }
        } else {
            PrefixResolution::NoMatch
        }
    }

    /// Iterates over entries with the given prefix.
    pub fn resolve_prefix_range<'a: 'b, 'b>(
        &'a self,
        prefix: &'b HexPrefix,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'b {
        let min_bytes = prefix.min_prefix_bytes();
        let pos = self.0.partition_point(|(k, _)| k.as_bytes() < min_bytes);
        self.0[pos..]
            .iter()
            .take_while(|(k, _)| prefix.matches(k))
            .map(|(k, v)| (k, v))
    }

    /// This function returns the shortest length of a prefix of `key` that
    /// disambiguates it from every other key in the index.
    ///
    /// The length to be returned is a number of hexadecimal digits.
    ///
    /// This has some properties that we do not currently make much use of:
    ///
    /// - The algorithm works even if `key` itself is not in the index.
    ///
    /// - In the special case when there are keys in the trie for which our
    ///   `key` is an exact prefix, returns `key.len() + 1`. Conceptually, in
    ///   order to disambiguate, you need every letter of the key *and* the
    ///   additional fact that it's the entire key). This case is extremely
    ///   unlikely for hashes with 12+ hexadecimal characters.
    pub fn shortest_unique_prefix_len(&self, key: &K) -> usize {
        let pos = self.0.partition_point(|(k, _)| k < key);
        let left = pos.checked_sub(1).map(|p| &self.0[p]);
        let right = self.0[pos..].iter().find(|(k, _)| k != key);
        itertools::chain(left, right)
            .map(|(neighbor, _value)| {
                backend::common_hex_len(key.as_bytes(), neighbor.as_bytes()) + 1
            })
            .max()
            .unwrap_or(0)
    }
}

struct EagerRevset<'index> {
    index_entries: Vec<IndexEntry<'index>>,
}

impl EagerRevset<'static> {
    pub const fn empty() -> Self {
        EagerRevset {
            index_entries: Vec::new(),
        }
    }
}

impl<'index> InternalRevset<'index> for EagerRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(self.index_entries.iter().cloned())
    }
}

impl<'index> ToPredicateFn<'index> for EagerRevset<'index> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        predicate_fn_from_iter(self.iter())
    }
}

struct RevWalkRevset<'index, T>
where
    // RevWalkRevset<'index> appears to be needed to assert 'index outlives 'a
    // in to_predicate_fn<'a>(&'a self) -> Box<dyn 'a>.
    T: Iterator<Item = IndexEntry<'index>>,
{
    walk: T,
}

impl<'index, T> InternalRevset<'index> for RevWalkRevset<'index, T>
where
    T: Iterator<Item = IndexEntry<'index>> + Clone,
{
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(self.walk.clone())
    }
}

impl<'index, T> ToPredicateFn<'index> for RevWalkRevset<'index, T>
where
    T: Iterator<Item = IndexEntry<'index>> + Clone,
{
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        predicate_fn_from_iter(self.iter())
    }
}

fn predicate_fn_from_iter<'index, 'iter>(
    iter: impl Iterator<Item = IndexEntry<'index>> + 'iter,
) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + 'iter> {
    let mut iter = iter.fuse().peekable();
    Box::new(move |entry| {
        while iter.next_if(|e| e.position() > entry.position()).is_some() {
            continue;
        }
        match iter.peek() {
            Some(e) if e.position() == entry.position() => {
                iter.next();
                PredicateMatch::Match
            }
            Some(_) => PredicateMatch::NotThisOne,
            None => PredicateMatch::NeverAgain,
        }
    })
}

struct ChildrenRevset<'index> {
    // The revisions we want to find children for
    root_set: Box<dyn InternalRevset<'index> + 'index>,
    // Consider only candidates from this set
    candidate_set: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for ChildrenRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        let roots: HashSet<_> = self
            .root_set
            .iter()
            .map(|parent| parent.position())
            .collect();

        Box::new(self.candidate_set.iter().filter(move |candidate| {
            candidate
                .parent_positions()
                .iter()
                .any(|parent_pos| roots.contains(parent_pos))
        }))
    }
}

impl<'index> ToPredicateFn<'index> for ChildrenRevset<'index> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        // TODO: can be optimized if candidate_set contains all heads
        predicate_fn_from_iter(self.iter())
    }
}

struct FilterRevset<'index, P> {
    candidates: Box<dyn InternalRevset<'index> + 'index>,
    predicate: P,
}

impl<'index, P> InternalRevset<'index> for FilterRevset<'index, P>
where
    P: ToPredicateFn<'index>,
{
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(FilterRevsetIterator {
            candidates_iter: self.candidates.iter().fuse(),
            predicate: self.predicate.to_predicate_fn(),
        })
    }
}

struct FilterRevsetIterator<I, P> {
    candidates_iter: I,
    predicate: P,
}

impl<'index, I, P> Iterator for FilterRevsetIterator<I, P>
where
    I: Iterator<Item = IndexEntry<'index>>,
    P: FnMut(&IndexEntry<'index>) -> PredicateMatch,
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        for entry in self.candidates_iter.by_ref() {
            match (self.predicate)(&entry) {
                PredicateMatch::Match => return Some(entry),
                PredicateMatch::NotThisOne => continue,
                PredicateMatch::NeverAgain => break,
            }
        }
        None
    }
}

impl<'index, P> ToPredicateFn<'index> for FilterRevset<'index, P>
where
    P: ToPredicateFn<'index>,
{
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        // TODO: optimize 'p1' out if candidates = All
        let mut p1 = self.candidates.to_predicate_fn();
        let mut p2 = self.predicate.to_predicate_fn();
        Box::new(move |entry| p1(entry).and(p2(entry)))
    }
}

struct UnionRevset<'index> {
    set1: Box<dyn InternalRevset<'index> + 'index>,
    set2: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for UnionRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(UnionRevsetIterator {
            iter1: self.set1.iter().peekable(),
            iter2: self.set2.iter().peekable(),
        })
    }
}

impl<'index> ToPredicateFn<'index> for UnionRevset<'index> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry).or(p2(entry)))
    }
}

struct UnionRevsetIterator<
    'index,
    I1: Iterator<Item = IndexEntry<'index>>,
    I2: Iterator<Item = IndexEntry<'index>>,
> {
    iter1: Peekable<I1>,
    iter2: Peekable<I2>,
}

impl<'index, I1: Iterator<Item = IndexEntry<'index>>, I2: Iterator<Item = IndexEntry<'index>>>
    Iterator for UnionRevsetIterator<'index, I1, I2>
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.iter1.peek(), self.iter2.peek()) {
            (None, _) => self.iter2.next(),
            (_, None) => self.iter1.next(),
            (Some(entry1), Some(entry2)) => match entry1.position().cmp(&entry2.position()) {
                Ordering::Less => self.iter2.next(),
                Ordering::Equal => {
                    self.iter1.next();
                    self.iter2.next()
                }
                Ordering::Greater => self.iter1.next(),
            },
        }
    }
}

struct DifferenceRevset<'index> {
    // The minuend (what to subtract from)
    set1: Box<dyn InternalRevset<'index> + 'index>,
    // The subtrahend (what to subtract)
    set2: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for DifferenceRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(DifferenceRevsetIterator {
            iter1: self.set1.iter().peekable(),
            iter2: self.set2.iter().peekable(),
        })
    }
}

impl<'index> ToPredicateFn<'index> for DifferenceRevset<'index> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        // TODO: optimize 'p1' out for unary negate?
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry).and_not(p2(entry)))
    }
}

struct DifferenceRevsetIterator<
    'index,
    I1: Iterator<Item = IndexEntry<'index>>,
    I2: Iterator<Item = IndexEntry<'index>>,
> {
    iter1: Peekable<I1>,
    iter2: Peekable<I2>,
}

impl<'index, I1: Iterator<Item = IndexEntry<'index>>, I2: Iterator<Item = IndexEntry<'index>>>
    Iterator for DifferenceRevsetIterator<'index, I1, I2>
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match (self.iter1.peek(), self.iter2.peek()) {
                (None, _) => {
                    return None;
                }
                (_, None) => {
                    return self.iter1.next();
                }
                (Some(entry1), Some(entry2)) => match entry1.position().cmp(&entry2.position()) {
                    Ordering::Less => {
                        self.iter2.next();
                    }
                    Ordering::Equal => {
                        self.iter2.next();
                        self.iter1.next();
                    }
                    Ordering::Greater => {
                        return self.iter1.next();
                    }
                },
            }
        }
    }
}

// TODO: Having to pass both `&dyn Index` and `CompositeIndex` is a bit ugly.
// Maybe we should make `CompositeIndex` implement `Index`?
pub fn evaluate<'index>(
    expression: &RevsetExpression,
    store: &Arc<Store>,
    index: &'index dyn Index,
    composite_index: CompositeIndex<'index>,
    visible_heads: &[CommitId],
) -> Result<RevsetImpl<'index>, RevsetError> {
    let context = EvaluationContext {
        store: store.clone(),
        index,
        composite_index: composite_index.clone(),
        visible_heads,
    };
    let internal_revset = context.evaluate(expression)?;
    Ok(RevsetImpl::new(internal_revset, composite_index))
}

struct EvaluationContext<'index, 'heads> {
    store: Arc<Store>,
    index: &'index dyn Index,
    composite_index: CompositeIndex<'index>,
    visible_heads: &'heads [CommitId],
}

impl<'index, 'heads> EvaluationContext<'index, 'heads> {
    fn evaluate(
        &self,
        expression: &RevsetExpression,
    ) -> Result<Box<dyn InternalRevset<'index> + 'index>, RevsetError> {
        match expression {
            RevsetExpression::Symbol(_)
            | RevsetExpression::Branches(_)
            | RevsetExpression::RemoteBranches { .. }
            | RevsetExpression::Tags
            | RevsetExpression::GitRefs
            | RevsetExpression::GitHead => {
                panic!("Expression '{expression:?}' should have been resolved by caller");
            }
            RevsetExpression::None => Ok(Box::new(EagerRevset::empty())),
            RevsetExpression::All => {
                // Since `all()` does not include hidden commits, some of the logical
                // transformation rules may subtly change the evaluated set. For example,
                // `all() & x` is not `x` if `x` is hidden. This wouldn't matter in practice,
                // but if it does, the heads set could be extended to include the commits
                // (and `remote_branches()`) specified in the revset expression. Alternatively,
                // some optimization rules could be removed, but that means `author(_) & x`
                // would have to test `:heads() & x`.
                self.evaluate(&RevsetExpression::visible_heads().ancestors())
            }
            RevsetExpression::Commits(commit_ids) => Ok(self.revset_for_commit_ids(commit_ids)),
            RevsetExpression::Children(roots) => {
                let root_set = self.evaluate(roots)?;
                let candidates_expression = roots.descendants();
                let candidate_set = self.evaluate(&candidates_expression)?;
                Ok(Box::new(ChildrenRevset {
                    root_set,
                    candidate_set,
                }))
            }
            RevsetExpression::Ancestors { heads, generation } => {
                let range_expression = RevsetExpression::Range {
                    roots: RevsetExpression::none(),
                    heads: heads.clone(),
                    generation: generation.clone(),
                };
                self.evaluate(&range_expression)
            }
            RevsetExpression::Range {
                roots,
                heads,
                generation,
            } => {
                let root_set = self.evaluate(roots)?;
                let root_ids = root_set.iter().map(|entry| entry.commit_id()).collect_vec();
                let head_set = self.evaluate(heads)?;
                let head_ids = head_set.iter().map(|entry| entry.commit_id()).collect_vec();
                let walk = self.composite_index.walk_revs(&head_ids, &root_ids);
                if generation == &GENERATION_RANGE_FULL {
                    Ok(Box::new(RevWalkRevset { walk }))
                } else {
                    let walk = walk.filter_by_generation(generation.clone());
                    Ok(Box::new(RevWalkRevset { walk }))
                }
            }
            RevsetExpression::DagRange { roots, heads } => {
                let root_set = self.evaluate(roots)?;
                let candidate_set = self.evaluate(&heads.ancestors())?;
                let mut reachable: HashSet<_> =
                    root_set.iter().map(|entry| entry.position()).collect();
                let mut result = vec![];
                let candidates = candidate_set.iter().collect_vec();
                for candidate in candidates.into_iter().rev() {
                    if reachable.contains(&candidate.position())
                        || candidate
                            .parent_positions()
                            .iter()
                            .any(|parent_pos| reachable.contains(parent_pos))
                    {
                        reachable.insert(candidate.position());
                        result.push(candidate);
                    }
                }
                result.reverse();
                Ok(Box::new(EagerRevset {
                    index_entries: result,
                }))
            }
            RevsetExpression::VisibleHeads => Ok(self.revset_for_commit_ids(self.visible_heads)),
            RevsetExpression::Heads(candidates) => {
                let candidate_set = self.evaluate(candidates)?;
                let candidate_ids = candidate_set
                    .iter()
                    .map(|entry| entry.commit_id())
                    .collect_vec();
                Ok(self
                    .revset_for_commit_ids(&self.composite_index.heads(&mut candidate_ids.iter())))
            }
            RevsetExpression::Roots(candidates) => {
                let connected_set = self.evaluate(&candidates.connected())?;
                let filled: HashSet<_> =
                    connected_set.iter().map(|entry| entry.position()).collect();
                let mut index_entries = vec![];
                let candidate_set = self.evaluate(candidates)?;
                for candidate in candidate_set.iter() {
                    if !candidate
                        .parent_positions()
                        .iter()
                        .any(|parent| filled.contains(parent))
                    {
                        index_entries.push(candidate);
                    }
                }
                Ok(Box::new(EagerRevset { index_entries }))
            }
            RevsetExpression::Latest { candidates, count } => {
                let candidate_set = self.evaluate(candidates)?;
                Ok(self.take_latest_revset(candidate_set.as_ref(), *count))
            }
            RevsetExpression::Filter(predicate) => Ok(Box::new(FilterRevset {
                candidates: self.evaluate(&RevsetExpression::All)?,
                predicate: build_predicate_fn(self.store.clone(), self.index, predicate),
            })),
            RevsetExpression::AsFilter(candidates) => self.evaluate(candidates),
            RevsetExpression::Present(candidates) => match self.evaluate(candidates) {
                Ok(set) => Ok(set),
                Err(RevsetError::NoSuchRevision(_)) => Ok(Box::new(EagerRevset::empty())),
                r @ Err(RevsetError::AmbiguousIdPrefix(_) | RevsetError::StoreError(_)) => r,
            },
            RevsetExpression::NotIn(complement) => {
                let set1 = self.evaluate(&RevsetExpression::All)?;
                let set2 = self.evaluate(complement)?;
                Ok(Box::new(DifferenceRevset { set1, set2 }))
            }
            RevsetExpression::Union(expression1, expression2) => {
                let set1 = self.evaluate(expression1)?;
                let set2 = self.evaluate(expression2)?;
                Ok(Box::new(UnionRevset { set1, set2 }))
            }
            RevsetExpression::Intersection(expression1, expression2) => {
                match expression2.as_ref() {
                    expression2 => Ok(Box::new(FilterRevset {
                        candidates: self.evaluate(expression1)?,
                        predicate: self.evaluate(expression2)?,
                    })),
                }
            }
            RevsetExpression::Difference(expression1, expression2) => {
                let set1 = self.evaluate(expression1)?;
                let set2 = self.evaluate(expression2)?;
                Ok(Box::new(DifferenceRevset { set1, set2 }))
            }
        }
    }

    fn revset_for_commit_ids(
        &self,
        commit_ids: &[CommitId],
    ) -> Box<dyn InternalRevset<'index> + 'index> {
        let mut index_entries = vec![];
        for id in commit_ids {
            index_entries.push(self.composite_index.entry_by_id(id).unwrap());
        }
        index_entries.sort_unstable_by_key(|b| Reverse(b.position()));
        index_entries.dedup();
        Box::new(EagerRevset { index_entries })
    }

    fn take_latest_revset(
        &self,
        candidate_set: &dyn InternalRevset<'index>,
        count: usize,
    ) -> Box<dyn InternalRevset<'index> + 'index> {
        if count == 0 {
            return Box::new(EagerRevset::empty());
        }

        #[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
        struct Item<'a> {
            timestamp: MillisSinceEpoch,
            entry: IndexEntryByPosition<'a>, // tie-breaker
        }

        let make_rev_item = |entry: IndexEntry<'index>| {
            let commit = self.store.get_commit(&entry.commit_id()).unwrap();
            Reverse(Item {
                timestamp: commit.committer().timestamp.timestamp.clone(),
                entry: IndexEntryByPosition(entry),
            })
        };

        // Maintain min-heap containing the latest (greatest) count items. For small
        // count and large candidate set, this is probably cheaper than building vec
        // and applying selection algorithm.
        let mut candidate_iter = candidate_set.iter().map(make_rev_item).fuse();
        let mut latest_items = BinaryHeap::from_iter(candidate_iter.by_ref().take(count));
        for item in candidate_iter {
            let mut earliest = latest_items.peek_mut().unwrap();
            if earliest.0 < item.0 {
                *earliest = item;
            }
        }

        assert!(latest_items.len() <= count);
        let mut index_entries = latest_items
            .into_iter()
            .map(|item| item.0.entry.0)
            .collect_vec();
        index_entries.sort_unstable_by_key(|b| Reverse(b.position()));
        Box::new(EagerRevset { index_entries })
    }
}

type PurePredicateFn<'index> = Box<dyn Fn(&IndexEntry<'index>) -> PredicateMatch + 'index>;

impl<'index> ToPredicateFn<'index> for PurePredicateFn<'index> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'index>) -> PredicateMatch + '_> {
        Box::new(self)
    }
}

fn build_predicate_fn<'index>(
    store: Arc<Store>,
    index: &'index dyn Index,
    predicate: &RevsetFilterPredicate,
) -> PurePredicateFn<'index> {
    match predicate {
        RevsetFilterPredicate::ParentCount(parent_count_range) => {
            let parent_count_range = parent_count_range.clone();
            Box::new(move |entry| {
                PredicateMatch::from_boolean(parent_count_range.contains(&entry.num_parents()))
            })
        }
        RevsetFilterPredicate::Description(needle) => {
            let needle = needle.clone();
            Box::new(move |entry| {
                PredicateMatch::from_boolean(
                    store
                        .get_commit(&entry.commit_id())
                        .unwrap()
                        .description()
                        .contains(needle.as_str()),
                )
            })
        }
        RevsetFilterPredicate::Author(needle) => {
            let needle = needle.clone();
            // TODO: Make these functions that take a needle to search for accept some
            // syntax for specifying whether it's a regex and whether it's
            // case-sensitive.
            Box::new(move |entry| {
                let commit = store.get_commit(&entry.commit_id()).unwrap();
                PredicateMatch::from_boolean(
                    commit.author().name.contains(needle.as_str())
                        || commit.author().email.contains(needle.as_str()),
                )
            })
        }
        RevsetFilterPredicate::Committer(needle) => {
            let needle = needle.clone();
            Box::new(move |entry| {
                let commit = store.get_commit(&entry.commit_id()).unwrap();
                PredicateMatch::from_boolean(
                    commit.committer().name.contains(needle.as_str())
                        || commit.committer().email.contains(needle.as_str()),
                )
            })
        }
        RevsetFilterPredicate::File(paths) => {
            // TODO: Add support for globs and other formats
            let matcher: Box<dyn Matcher> = if let Some(paths) = paths {
                Box::new(PrefixMatcher::new(paths))
            } else {
                Box::new(EverythingMatcher)
            };
            Box::new(move |entry| {
                PredicateMatch::from_boolean(has_diff_from_parent(
                    &store,
                    index,
                    entry,
                    matcher.as_ref(),
                ))
            })
        }
    }
}

fn has_diff_from_parent(
    store: &Arc<Store>,
    index: &dyn Index,
    entry: &IndexEntry<'_>,
    matcher: &dyn Matcher,
) -> bool {
    let commit = store.get_commit(&entry.commit_id()).unwrap();
    let parents = commit.parents();
    let from_tree = rewrite::merge_commit_trees_without_repo(store, index, &parents);
    let to_tree = commit.tree();
    from_tree.diff(&to_tree, matcher).next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChangeId, CommitId, ObjectId};
    use crate::default_index_store::MutableIndexImpl;
    use crate::index::Index;

    #[test]
    fn test_id_index_resolve_prefix() {
        fn sorted(resolution: PrefixResolution<Vec<i32>>) -> PrefixResolution<Vec<i32>> {
            match resolution {
                PrefixResolution::SingleMatch(mut xs) => {
                    xs.sort(); // order of values might not be preserved by IdIndex
                    PrefixResolution::SingleMatch(xs)
                }
                _ => resolution,
            }
        }
        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("0000"), 0),
            (ChangeId::from_hex("0099"), 1),
            (ChangeId::from_hex("0099"), 2),
            (ChangeId::from_hex("0aaa"), 3),
            (ChangeId::from_hex("0aab"), 4),
        ]);
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("00").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("000").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![0]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0001").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
        assert_eq!(
            sorted(id_index.resolve_prefix_with(&HexPrefix::new("009").unwrap(), |&v| v)),
            PrefixResolution::SingleMatch(vec![1, 2]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aa").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aab").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![4]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("f").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
    }

    #[test]
    fn test_id_index_shortest_unique_prefix_len() {
        // No crash if empty
        let id_index = IdIndex::from_vec(vec![] as Vec<(ChangeId, ())>);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("00")),
            0
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acd0"), ()), // duplicated key is allowed
        ]);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ac")),
            3
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acf0"), ()),
            (ChangeId::from_hex("a0"), ()),
            (ChangeId::from_hex("ba"), ()),
        ]);

        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("a0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ba")),
            1
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ab")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            3
        );
        // If it were there, the length would be 1.
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("c0")),
            1
        );
    }

    /// Generator of unique 16-byte ChangeId excluding root id
    fn change_id_generator() -> impl FnMut() -> ChangeId {
        let mut iter = (1_u128..).map(|n| ChangeId::new(n.to_le_bytes().into()));
        move || iter.next().unwrap()
    }

    #[test]
    fn test_revset_combinator() {
        let mut new_change_id = change_id_generator();
        let mut index = MutableIndexImpl::full(3, 16);
        let id_0 = CommitId::from_hex("000000");
        let id_1 = CommitId::from_hex("111111");
        let id_2 = CommitId::from_hex("222222");
        let id_3 = CommitId::from_hex("333333");
        let id_4 = CommitId::from_hex("444444");
        index.add_commit_data(id_0.clone(), new_change_id(), &[]);
        index.add_commit_data(id_1.clone(), new_change_id(), &[id_0.clone()]);
        index.add_commit_data(id_2.clone(), new_change_id(), &[id_1.clone()]);
        index.add_commit_data(id_3.clone(), new_change_id(), &[id_2.clone()]);
        index.add_commit_data(id_4.clone(), new_change_id(), &[id_3.clone()]);

        let get_entry = |id: &CommitId| index.entry_by_id(id).unwrap();
        let make_entries = |ids: &[&CommitId]| ids.iter().map(|id| get_entry(id)).collect_vec();
        let make_set = |ids: &[&CommitId]| -> Box<dyn InternalRevset> {
            let index_entries = make_entries(ids);
            Box::new(EagerRevset { index_entries })
        };

        let set = make_set(&[&id_4, &id_3, &id_2, &id_0]);
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::Match);
        // Uninteresting entries can be skipped
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::Match);

        let set = FilterRevset::<PurePredicateFn> {
            candidates: make_set(&[&id_4, &id_2, &id_0]),
            predicate: Box::new(|entry| PredicateMatch::from_boolean(entry.commit_id() != id_4)),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2, &id_0]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::Match);

        // Intersection by FilterRevset
        let set = FilterRevset {
            candidates: make_set(&[&id_4, &id_2, &id_0]),
            predicate: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);

        // Intersection terminates early (but it's still okay to ask again)
        let set = FilterRevset {
            candidates: make_set(&[&id_4, &id_3]),
            predicate: make_set(&[&id_3, &id_2]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_3]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::NeverAgain);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NeverAgain);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);

        let set = UnionRevset {
            set1: make_set(&[&id_4, &id_2]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(
            set.iter().collect_vec(),
            make_entries(&[&id_4, &id_3, &id_2, &id_1])
        );
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);

        // Intersection terminates early (but it's still okay to ask again)
        let set = UnionRevset {
            set1: make_set(&[&id_4, &id_2]),
            set2: make_set(&[&id_3, &id_2]),
        };
        assert_eq!(
            set.iter().collect_vec(),
            make_entries(&[&id_4, &id_3, &id_2])
        );
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NeverAgain);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);

        let set = FilterRevset {
            candidates: make_set(&[&id_4, &id_2, &id_0]),
            predicate: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);

        let set = DifferenceRevset {
            set1: make_set(&[&id_4, &id_2, &id_0]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_4, &id_0]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::Match);

        // Difference terminates early (but it's still okay to ask again)
        let set = DifferenceRevset {
            set1: make_set(&[&id_4, &id_3]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_4]));
        let mut p = set.to_predicate_fn();
        assert_eq!(p(&get_entry(&id_4)), PredicateMatch::Match);
        assert_eq!(p(&get_entry(&id_3)), PredicateMatch::NotThisOne);
        assert_eq!(p(&get_entry(&id_2)), PredicateMatch::NeverAgain);
        assert_eq!(p(&get_entry(&id_1)), PredicateMatch::NeverAgain);
        assert_eq!(p(&get_entry(&id_0)), PredicateMatch::NeverAgain);
    }
}
