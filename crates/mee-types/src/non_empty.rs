//! A correct-by-construction non-empty collection.
//!
//! API mirrors the [`nonempty`](https://crates.io/crates/nonempty) crate so
//! we can swap to the external dep later if needed.

use serde::de::Deserializer;
use serde::ser::{SerializeSeq, Serializer};

/// A non-empty Vec. Guaranteed to contain at least one element by
/// construction — no runtime checks required at usage sites.
///
/// # Layout
///
/// `head` is always present; `tail` holds zero or more additional items.
/// Matches the `nonempty` crate's public-field convention.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NonEmpty<T> {
    /// The first (guaranteed) element.
    pub head: T,
    /// Remaining elements (may be empty).
    pub tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    /// Create a single-element collection.
    pub const fn new(head: T) -> Self {
        Self {
            head,
            tail: Vec::new(),
        }
    }

    /// Build from a `Vec`, returning `None` if it was empty.
    pub fn from_vec(vec: Vec<T>) -> Option<Self> {
        let mut iter = vec.into_iter();
        let head = iter.next()?;
        Some(Self {
            head,
            tail: iter.collect(),
        })
    }

    /// Returns a reference to the first element (always exists).
    pub const fn first(&self) -> &T {
        &self.head
    }

    /// Returns a reference to the last element.
    pub fn last(&self) -> &T {
        self.tail.last().unwrap_or(&self.head)
    }

    /// Total number of elements (always >= 1).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        1 + self.tail.len()
    }

    /// Iterate over all elements in order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }

    /// Append an element.
    pub fn push(&mut self, item: T) {
        self.tail.push(item);
    }

    /// Returns `true` if the collection contains the given item.
    pub fn contains(&self, item: &T) -> bool
    where
        T: PartialEq,
    {
        self.head == *item || self.tail.contains(item)
    }

    /// Remove the first element matching `predicate`.
    ///
    /// Returns `Err(())` if no element matches or if removing the
    /// match would leave the collection empty (i.e. the match is the
    /// only element).
    #[allow(clippy::result_unit_err)] // Callers map to domain error
    pub fn try_remove<F>(&mut self, predicate: F) -> Result<T, ()>
    where
        F: Fn(&T) -> bool,
    {
        if predicate(&self.head) {
            // Head matches — can only remove if tail is non-empty.
            if self.tail.is_empty() {
                return Err(()); // last element
            }
            let new_head = self.tail.remove(0);
            let removed = std::mem::replace(&mut self.head, new_head);
            return Ok(removed);
        }
        // Check tail.
        if let Some(pos) = self.tail.iter().position(&predicate) {
            Ok(self.tail.remove(pos))
        } else {
            Err(()) // not found
        }
    }

    /// Consume into a `Vec<T>`.
    pub fn into_vec(self) -> Vec<T> {
        let mut v = Vec::with_capacity(1 + self.tail.len());
        v.push(self.head);
        v.extend(self.tail);
        v
    }
}

// --- Trait impls ---

impl<T: std::fmt::Debug> std::fmt::Debug for NonEmpty<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(ne: NonEmpty<T>) -> Self {
        ne.into_vec()
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::iter::Chain<std::iter::Once<T>, std::vec::IntoIter<T>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.head).chain(self.tail)
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Chain<std::iter::Once<&'a T>, std::slice::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(&self.head).chain(self.tail.iter())
    }
}

// --- Serde: serializes as a flat JSON array, rejects empty on deser ---

impl<T: serde::Serialize> serde::Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        seq.serialize_element(&self.head)?;
        for item in &self.tail {
            seq.serialize_element(item)?;
        }
        seq.end()
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let vec = Vec::<T>::deserialize(deserializer)?;
        Self::from_vec(vec).ok_or_else(|| serde::de::Error::custom("expected non-empty array"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_single_element() {
        let ne = NonEmpty::new(42);
        assert_eq!(*ne.first(), 42);
        assert_eq!(ne.len(), 1);
    }

    #[test]
    fn from_vec_empty_returns_none() {
        let result = NonEmpty::<i32>::from_vec(vec![]);
        assert!(result.is_none());
    }

    #[test]
    fn from_vec_preserves_order() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        assert_eq!(ne.head, 1);
        assert_eq!(ne.tail, vec![2, 3]);
    }

    #[test]
    fn first_and_last() {
        let ne = NonEmpty::from_vec(vec![10, 20, 30]).unwrap();
        assert_eq!(*ne.first(), 10);
        assert_eq!(*ne.last(), 30);
    }

    #[test]
    fn last_single_element() {
        let ne = NonEmpty::new(7);
        assert_eq!(*ne.last(), 7);
    }

    #[test]
    fn push_and_len() {
        let mut ne = NonEmpty::new(1);
        ne.push(2);
        ne.push(3);
        assert_eq!(ne.len(), 3);
        assert_eq!(ne.tail, vec![2, 3]);
    }

    #[test]
    fn iter_order() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        let collected: Vec<_> = ne.iter().copied().collect();
        assert_eq!(collected, vec![1, 2, 3]);
    }

    #[test]
    fn into_vec_roundtrip() {
        let original = vec![10, 20, 30];
        let ne = NonEmpty::from_vec(original.clone()).unwrap();
        assert_eq!(ne.into_vec(), original);
    }

    #[test]
    fn into_iterator() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        let collected: Vec<_> = ne.into_iter().collect();
        assert_eq!(collected, vec![1, 2, 3]);
    }

    #[test]
    fn debug_format() {
        let ne = NonEmpty::from_vec(vec![1, 2]).unwrap();
        let s = format!("{ne:?}");
        assert_eq!(s, "[1, 2]");
    }

    #[test]
    fn serde_roundtrip() {
        let ne = NonEmpty::from_vec(vec![1u32, 2, 3]).unwrap();
        let json = serde_json::to_string(&ne).unwrap();
        assert_eq!(json, "[1,2,3]");
        let back: NonEmpty<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ne);
    }

    #[test]
    fn serde_rejects_empty_array() {
        let result = serde_json::from_str::<NonEmpty<u32>>("[]");
        assert!(result.is_err());
    }

    #[test]
    fn equality() {
        let a = NonEmpty::from_vec(vec![1, 2]).unwrap();
        let b = NonEmpty::from_vec(vec![1, 2]).unwrap();
        let c = NonEmpty::from_vec(vec![1, 3]).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // -- contains --

    #[test]
    fn contains_head() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        assert!(ne.contains(&1));
    }

    #[test]
    fn contains_tail() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        assert!(ne.contains(&3));
    }

    #[test]
    fn contains_missing() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        assert!(!ne.contains(&99));
    }

    // -- try_remove --

    #[test]
    fn try_remove_from_head() {
        let mut ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        let removed = ne.try_remove(|x| *x == 1).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(ne.head, 2);
        assert_eq!(ne.tail, vec![3]);
    }

    #[test]
    fn try_remove_from_tail() {
        let mut ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        let removed = ne.try_remove(|x| *x == 2).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(ne.head, 1);
        assert_eq!(ne.tail, vec![3]);
    }

    #[test]
    fn try_remove_last_element_fails() {
        let mut ne = NonEmpty::new(42);
        assert!(ne.try_remove(|x| *x == 42).is_err());
        assert_eq!(ne.len(), 1); // unchanged
    }

    #[test]
    fn try_remove_not_found() {
        let mut ne = NonEmpty::from_vec(vec![1, 2]).unwrap();
        assert!(ne.try_remove(|x| *x == 99).is_err());
        assert_eq!(ne.len(), 2); // unchanged
    }

    #[test]
    fn try_remove_head_promotes_tail() {
        let mut ne = NonEmpty::from_vec(vec![10, 20]).unwrap();
        let removed = ne.try_remove(|x| *x == 10).unwrap();
        assert_eq!(removed, 10);
        assert_eq!(ne.head, 20);
        assert!(ne.tail.is_empty());
        assert_eq!(ne.len(), 1);
    }
}
