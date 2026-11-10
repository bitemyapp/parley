// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Query support.

use crate::{Charmap, CharmapIndex};

use super::super::{Collection, SourceCache};

use alloc::vec::Vec;
use parlance::Script;

use super::{
    super::{Attributes, Blob, FallbackKey, FamilyId, FamilyInfo, GenericFamily, Synthesis},
    Inner,
};

#[derive(Clone, Default)]
pub(super) struct QueryState {
    families: FamilyList,
    fallback_families: Vec<CachedFamily>,
}

impl QueryState {
    fn clear(&mut self) {
        self.families.truncate(0);
        self.fallback_families.clear();
    }
}

/// The families a query matches against first, and what it has found out
/// about the ones it has reached.
///
/// The list can stand for hundreds of families: a generic family is as
/// long as the system makes it, and fontconfig's `sans-serif` and `emoji`
/// commonly run past a hundred. Matching rarely goes beyond the first few,
/// so a family costs nothing until matching reaches it: a generic family
/// is expanded then, and a family gets its cache entry when it is first
/// looked at.
#[derive(Clone, Default)]
struct FamilyList {
    /// The list as it was set, with family names resolved.
    sources: Vec<Source>,
    /// Where the families of each expanded source end in `ids`. Sources
    /// are expanded in order, so these are the first `ends.len()` of them.
    ends: Vec<usize>,
    /// The families of the expanded sources, in order.
    ids: Vec<FamilyId>,
    /// Cache entries for the first `cached.len()` families of `ids`.
    cached: Vec<CachedFamily>,
}

/// An entry of the list given to [`Query::set_families`].
#[derive(Copy, Clone, PartialEq, Eq)]
enum Source {
    Family(FamilyId),
    Generic(GenericFamily),
}

impl FamilyList {
    /// Shortens the list to its first `len` sources, keeping everything
    /// known about their families.
    fn truncate(&mut self, len: usize) {
        self.sources.truncate(len);
        if self.ends.len() > len {
            self.ends.truncate(len);
            let ids = self.ends.last().copied().unwrap_or(0);
            self.ids.truncate(ids);
            self.cached.truncate(ids);
        }
    }

    /// Returns the cache entry for the family at `index` of the expanded
    /// list, expanding sources as far as that takes, or `None` past the
    /// end of the list.
    ///
    /// Matching asks for each index in turn, starting from zero.
    fn family(&mut self, index: usize, collection: &mut Inner) -> Option<&mut CachedFamily> {
        while index >= self.ids.len() {
            match *self.sources.get(self.ends.len())? {
                Source::Family(id) => self.ids.push(id),
                Source::Generic(generic) => self.ids.extend(collection.generic_families(generic)),
            }
            self.ends.push(self.ids.len());
        }
        while self.cached.len() <= index {
            self.cached
                .push(CachedFamily::new(self.ids[self.cached.len()]));
        }
        self.cached.get_mut(index)
    }
}

/// State for font selection.
///
/// Instances of this can be obtained from [`Collection::query`].
pub struct Query<'a> {
    collection: &'a mut Inner,
    state: &'a mut QueryState,
    source_cache: &'a mut SourceCache,
    attributes: Attributes,
    fallbacks: Option<FallbackKey>,
}

impl<'a> Query<'a> {
    pub(super) fn new(collection: &'a mut Collection, source_cache: &'a mut SourceCache) -> Self {
        collection.query_state.clear();
        Self {
            collection: &mut collection.inner,
            state: &mut collection.query_state,
            source_cache,
            attributes: Attributes::default(),
            fallbacks: None,
        }
    }

    /// Sets the ordered sequence of families to match against.
    pub fn set_families<'f, I>(&mut self, families: I)
    where
        I: IntoIterator,
        I::Item: Into<QueryFamily<'f>>,
    {
        // The new list often starts as the old one did: the same families
        // with another after them, or without it. What the query has found
        // out about those families still holds, so only the part of the
        // list that differs is replaced.
        let list = &mut self.state.families;
        let mut len = 0;
        let mut same = true;
        for family in families {
            let source = match family.into() {
                QueryFamily::Named(name) => match self.collection.family_id(name) {
                    Some(id) => Source::Family(id),
                    None => continue,
                },
                QueryFamily::Id(id) => Source::Family(id),
                QueryFamily::Generic(generic) => Source::Generic(generic),
            };
            if same && list.sources.get(len) != Some(&source) {
                list.truncate(len);
                same = false;
            }
            if !same {
                list.sources.push(source);
            }
            len += 1;
        }
        list.truncate(len);
    }

    /// Sets the primary attributes to match against.
    pub fn set_attributes(&mut self, attributes: Attributes) {
        if self.attributes != attributes {
            for family in &mut self.state.families.cached {
                family.clear_fonts();
            }
            for family in &mut self.state.fallback_families {
                family.clear_fonts();
            }
            self.attributes = attributes;
        }
    }

    /// Sets the script and locale for fallback fonts.
    pub fn set_fallbacks(&mut self, key: impl Into<FallbackKey>) {
        let key = key.into();
        if self.fallbacks != Some(key) {
            self.state.fallback_families.clear();
            self.state.fallback_families.extend(
                self.collection
                    .fallback_families(key)
                    .map(CachedFamily::new),
            );
            // HACK: always add a Han font to the fallback list to capture
            // punctuation in the common script
            // See <https://github.com/linebender/parley/issues/597>
            self.state.fallback_families.extend(
                self.collection
                    .fallback_families(FallbackKey::new(Script::from_bytes(*b"Hani"), None))
                    .map(CachedFamily::new),
            );
            self.fallbacks = Some(key);
        }
    }

    /// Invokes the given callback with all fonts that match the current
    /// settings.
    ///
    /// Return [`QueryStatus::Stop`] to end iterating over the matching
    /// fonts or [`QueryStatus::Continue`] to continue iterating.
    pub fn matches_with(&mut self, mut f: impl FnMut(&QueryFont) -> QueryStatus) {
        let mut index = 0;
        loop {
            // The families that were set, then the fallback families.
            let families = &mut self.state.families;
            let family = match families.family(index, self.collection) {
                Some(family) => family,
                None => {
                    let fallback = index - families.ids.len();
                    match self.state.fallback_families.get_mut(fallback) {
                        Some(family) => family,
                        None => return,
                    }
                }
            };
            index += 1;
            match &mut family.family {
                Entry::Error => continue,
                Entry::Ok(..) => {}
                status @ Entry::Vacant => {
                    if let Some(info) = self.collection.family(family.id) {
                        *status = Entry::Ok(info);
                    } else {
                        *status = Entry::Error;
                        continue;
                    }
                }
            }
            let Entry::Ok(family_info) = &family.family else {
                continue;
            };
            let mut best_index = None;
            if let Some(font) = load_font(
                family_info,
                self.attributes,
                &mut family.best,
                false,
                self.source_cache,
            ) {
                best_index = Some(font.family.1);
                if f(font) == QueryStatus::Stop {
                    return;
                }
            }
            // Don't invoke for the default font if it's the same as the
            // best match.
            if best_index == Some(family_info.default_font_index()) {
                continue;
            }
            if let Some(font) = load_font(
                family_info,
                self.attributes,
                &mut family.default,
                true,
                self.source_cache,
            ) {
                if f(font) == QueryStatus::Stop {
                    return;
                }
            }
        }
    }
}

impl Drop for Query<'_> {
    fn drop(&mut self) {
        self.state.clear();
    }
}

/// Determines whether a font query operation will continue.
///
/// See [`Query::matches_with`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QueryStatus {
    /// Query should continue with the next font.
    Continue,
    /// Query should stop.
    Stop,
}

/// Family descriptor for a font query.
///
/// This allows [`Query::set_families`] to
/// take a variety of family types.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum QueryFamily<'a> {
    /// A named font family.
    Named(&'a str),
    /// An identifier for a font family.
    Id(FamilyId),
    /// A generic font family.
    Generic(GenericFamily),
}

impl<'a> From<&'a str> for QueryFamily<'a> {
    fn from(value: &'a str) -> Self {
        Self::Named(value)
    }
}

impl From<FamilyId> for QueryFamily<'static> {
    fn from(value: FamilyId) -> Self {
        Self::Id(value)
    }
}

impl From<GenericFamily> for QueryFamily<'static> {
    fn from(value: GenericFamily) -> Self {
        Self::Generic(value)
    }
}

/// Candidate font generated by a [`Query`].
#[derive(Clone, Debug)]
pub struct QueryFont {
    /// Family identifier and index of the font in the family font list.
    pub family: (FamilyId, usize),
    /// Blob containing the font data.
    pub blob: Blob<u8>,
    /// Index of a font in a font collection (`ttc`) file.
    pub index: u32,
    /// Synthesis suggestions for this font based on the requested attributes.
    pub synthesis: Synthesis,
    /// Data used for constructing a character map for this font.
    pub charmap_index: CharmapIndex,
}

impl QueryFont {
    /// Attempts to construct a [Charmap] for this font.
    pub fn charmap(&self) -> Option<Charmap<'_>> {
        self.charmap_index.charmap(self.blob.as_ref())
    }
}

fn load_font<'a>(
    family: &FamilyInfo,
    attributes: Attributes,
    font: &'a mut Entry<QueryFont>,
    is_default: bool,
    source_cache: &mut SourceCache,
) -> Option<&'a QueryFont> {
    match font {
        Entry::Error => None,
        Entry::Ok(font) => Some(font),
        status @ Entry::Vacant => {
            // Set to error in case we fail. This simplifies
            // the following code.
            *status = Entry::Error;
            let family_index = if is_default {
                family.default_font_index()
            } else {
                family.match_index(attributes.width, attributes.style, attributes.weight, true)?
            };
            let font_info = family.fonts().get(family_index)?;
            let blob = font_info.load(Some(source_cache))?;
            let blob_index = font_info.index();
            let synthesis =
                font_info.synthesis(attributes.width, attributes.style, attributes.weight);
            *status = Entry::Ok(QueryFont {
                family: (family.id(), family_index),
                blob: blob.clone(),
                index: blob_index,
                synthesis,
                charmap_index: font_info.charmap_index(),
            });
            if let Entry::Ok(font) = status {
                Some(font)
            } else {
                None
            }
        }
    }
}

#[derive(Clone)]
struct CachedFamily {
    id: FamilyId,
    family: Entry<FamilyInfo>,
    best: Entry<QueryFont>,
    default: Entry<QueryFont>,
}

impl CachedFamily {
    fn new(id: FamilyId) -> Self {
        Self {
            id,
            family: Entry::Vacant,
            best: Entry::Vacant,
            default: Entry::Vacant,
        }
    }

    fn clear_fonts(&mut self) {
        self.best = Entry::Vacant;
        self.default = Entry::Vacant;
    }
}

#[derive(Clone)]
enum Entry<T> {
    Ok(T),
    Vacant,
    Error,
}
