//! Per-activation RDP pointer cache (MS-RDPBCGR 2.2.9.1.1.4.5, Cached Pointer
//! Update).
//!
//! A full New Pointer or Large Pointer update stores its shape in the client's
//! pointer cache slot `cache_index`; a later Cached Pointer update reselects
//! that slot without resending pixels. Both updates share the client cache
//! whose size the client advertises as `pointerCacheSize` (2.2.7.1.5).
//!
//! The cache must mirror exactly what the client received, so it belongs to
//! the per-activation display-update stream: a new `Updates` stream starts with
//! an empty cache, which covers reconnects and deactivation-reactivation
//! (capability renegotiation, resize). Shapes are keyed by a hash of the
//! geometry, hotspot and pixels; a matching hash is only a hit after a full
//! comparison, so a collision can never select the wrong shape.
use ironrdp_server::DisplayUpdate;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

/// Upper bound on retained slots regardless of the advertised cache size.
/// Each slot holds a shape of up to 384x384 BGRA; the working set of distinct
/// cursors in a session is small, so a larger cache only costs memory.
const MAX_SLOTS: u16 = 32;

struct Slot {
    hash: u64,
    shape: Arc<DisplayUpdate>,
    last_used: u64,
}

pub(in crate::wayland) struct PointerCache {
    capacity: u16,
    slots: Vec<Slot>,
    clock: u64,
}

/// Identity of a pointer shape: kind, geometry, hotspot and pixels. The cache
/// index the shape was originally sent with is deliberately excluded.
type ShapeKey<'a> = (bool, u16, u16, u16, u16, &'a [u8]);

fn shape_key(update: &DisplayUpdate) -> Option<ShapeKey<'_>> {
    match update {
        DisplayUpdate::RGBAPointer(p) => {
            Some((false, p.width, p.height, p.hot_x, p.hot_y, &p.data[..]))
        }
        DisplayUpdate::LargePointer(p) => {
            Some((true, p.width, p.height, p.hot_x, p.hot_y, &p.data[..]))
        }
        _ => None,
    }
}

fn shape_hash(key: &ShapeKey<'_>) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn with_index(update: &DisplayUpdate, index: u16) -> DisplayUpdate {
    match update {
        DisplayUpdate::RGBAPointer(p) => DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: index,
            ..p.clone()
        }),
        DisplayUpdate::LargePointer(p) => {
            DisplayUpdate::LargePointer(ironrdp_server::LargePointer {
                cache_index: index,
                ..p.clone()
            })
        }
        other => other.clone(),
    }
}

impl PointerCache {
    /// `advertised` is the client's negotiated `pointerCacheSize`. Zero means
    /// the client has no New Pointer cache, or its size was not reported;
    /// shapes then pass through unchanged with cache index 0.
    pub(in crate::wayland) fn new(advertised: u16) -> Self {
        Self {
            capacity: advertised.min(MAX_SLOTS),
            slots: Vec::new(),
            clock: 0,
        }
    }

    /// Map an outgoing update onto the cache. A previously sent shape becomes
    /// `CachedPointer(index)`; a new shape is assigned a free or the least
    /// recently used slot and returned with that `cache_index`. Every other
    /// update is returned unchanged. The caller must send the result.
    pub(in crate::wayland) fn resolve(&mut self, shape: &Arc<DisplayUpdate>) -> DisplayUpdate {
        match shape_key(shape) {
            Some(key) if self.capacity != 0 => {
                let hash = shape_hash(&key);
                self.resolve_hashed(shape, hash)
            }
            _ => shape.as_ref().clone(),
        }
    }

    fn resolve_hashed(&mut self, shape: &Arc<DisplayUpdate>, hash: u64) -> DisplayUpdate {
        self.clock += 1;
        let key = shape_key(shape);
        let hit = self.slots.iter().position(|slot| {
            Arc::ptr_eq(&slot.shape, shape) || (slot.hash == hash && shape_key(&slot.shape) == key)
        });
        if let Some(index) = hit {
            self.slots[index].last_used = self.clock;
            return DisplayUpdate::CachedPointer(index as u16);
        }
        let slot = Slot {
            hash,
            shape: shape.clone(),
            last_used: self.clock,
        };
        let index = if self.slots.len() < usize::from(self.capacity) {
            self.slots.push(slot);
            self.slots.len() - 1
        } else {
            let (index, _) = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.last_used)
                .expect("nonzero capacity has a slot");
            self.slots[index] = slot;
            index
        };
        with_index(shape, index as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(seed: u8) -> Arc<DisplayUpdate> {
        Arc::new(DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: 0,
            width: 2,
            height: 1,
            hot_x: 1,
            hot_y: 0,
            data: vec![seed; 8],
        }))
    }

    fn large(seed: u8) -> Arc<DisplayUpdate> {
        Arc::new(DisplayUpdate::LargePointer(ironrdp_server::LargePointer {
            cache_index: 7,
            width: 97,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            data: vec![seed; 97 * 4],
        }))
    }

    /// Full update index, or `None` for a cached reference.
    fn sent_index(update: &DisplayUpdate) -> Option<u16> {
        match update {
            DisplayUpdate::RGBAPointer(p) => Some(p.cache_index),
            DisplayUpdate::LargePointer(p) => Some(p.cache_index),
            DisplayUpdate::CachedPointer(_) => None,
            _ => panic!("unexpected pointer update"),
        }
    }

    fn cached(update: &DisplayUpdate) -> Option<u16> {
        match update {
            DisplayUpdate::CachedPointer(index) => Some(*index),
            _ => None,
        }
    }

    #[test]
    fn pointer_cache_sends_new_shapes_in_free_slots_then_cached_references() {
        let mut cache = PointerCache::new(4);
        let (arrow, ibeam, hand) = (rgba(1), rgba(2), large(3));
        assert_eq!(sent_index(&cache.resolve(&arrow)), Some(0));
        assert_eq!(sent_index(&cache.resolve(&ibeam)), Some(1));
        let first_hand = cache.resolve(&hand);
        assert_eq!(sent_index(&first_hand), Some(2));
        let DisplayUpdate::LargePointer(sent) = &first_hand else {
            panic!("large shape must stay a large pointer");
        };
        assert_eq!(sent.data, vec![3; 97 * 4]);
        assert_eq!(cached(&cache.resolve(&arrow)), Some(0));
        assert_eq!(cached(&cache.resolve(&hand)), Some(2));
        // An equal shape in a fresh allocation (a repeated compositor Shape
        // event) is still a hit through full comparison.
        assert_eq!(cached(&cache.resolve(&rgba(2))), Some(1));
    }

    #[test]
    fn pointer_cache_evicts_least_recently_used_slot() {
        let mut cache = PointerCache::new(2);
        let (a, b, c) = (rgba(1), rgba(2), rgba(3));
        assert_eq!(sent_index(&cache.resolve(&a)), Some(0));
        assert_eq!(sent_index(&cache.resolve(&b)), Some(1));
        assert_eq!(cached(&cache.resolve(&a)), Some(0));
        // b is least recently used, so c takes its slot.
        assert_eq!(sent_index(&cache.resolve(&c)), Some(1));
        assert_eq!(cached(&cache.resolve(&a)), Some(0));
        assert_eq!(cached(&cache.resolve(&c)), Some(1));
        // b was evicted and must be sent in full again, replacing a.
        assert_eq!(cached(&cache.resolve(&c)), Some(1));
        assert_eq!(sent_index(&cache.resolve(&b)), Some(0));
        assert_eq!(sent_index(&cache.resolve(&a)), Some(1));
    }

    #[test]
    fn pointer_cache_of_a_new_activation_starts_empty() {
        let mut cache = PointerCache::new(4);
        let (a, b) = (rgba(1), rgba(2));
        cache.resolve(&a);
        cache.resolve(&b);
        assert_eq!(cached(&cache.resolve(&a)), Some(0));
        // A reconnect or reactivation builds a new cache: the same shapes
        // are sent in full again before any cached reference.
        let mut next = PointerCache::new(4);
        assert_eq!(sent_index(&next.resolve(&b)), Some(0));
        assert_eq!(sent_index(&next.resolve(&a)), Some(1));
        assert_eq!(cached(&next.resolve(&b)), Some(0));
    }

    #[test]
    fn pointer_cache_resolves_hash_collisions_by_full_comparison() {
        let mut cache = PointerCache::new(4);
        let (a, b) = (rgba(1), rgba(2));
        assert_eq!(sent_index(&cache.resolve_hashed(&a, 42)), Some(0));
        assert_eq!(sent_index(&cache.resolve_hashed(&b, 42)), Some(1));
        assert_eq!(cached(&cache.resolve_hashed(&rgba(1), 42)), Some(0));
        assert_eq!(cached(&cache.resolve_hashed(&rgba(2), 42)), Some(1));
        // Same pixels, different hotspot: a distinct shape.
        let moved = Arc::new(DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: 0,
            width: 2,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            data: vec![1; 8],
        }));
        assert_eq!(sent_index(&cache.resolve(&moved)), Some(2));
    }

    #[test]
    fn pointer_cache_bounds_slots_and_passes_through_without_client_cache() {
        let mut cache = PointerCache::new(u16::MAX);
        for seed in 0..=u8::try_from(MAX_SLOTS).unwrap() {
            let index = sent_index(&cache.resolve(&rgba(seed))).unwrap();
            assert!(index < MAX_SLOTS);
        }
        let mut disabled = PointerCache::new(0);
        let shape = rgba(1);
        for _ in 0..2 {
            assert_eq!(sent_index(&disabled.resolve(&shape)), Some(0));
        }
        let mut cache = PointerCache::new(1);
        assert!(matches!(
            cache.resolve(&Arc::new(DisplayUpdate::HidePointer)),
            DisplayUpdate::HidePointer
        ));
        assert_eq!(sent_index(&cache.resolve(&shape)), Some(0));
        assert_eq!(cached(&cache.resolve(&shape)), Some(0));
        assert_eq!(sent_index(&cache.resolve(&rgba(2))), Some(0));
    }
}
