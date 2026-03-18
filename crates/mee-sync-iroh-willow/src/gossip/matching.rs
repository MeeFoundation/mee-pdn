use std::collections::HashSet;
use std::hash::BuildHasher;

// TODO(bloom-filter): Replace set intersection with bloom filter probe.
// Accept bloom filter bytes + params instead of &[[u8; 32]].
// PAI catches false positives downstream.
/// Find namespace IDs present in both the received advertisement
/// and the local set of held capabilities.
pub fn intersect_namespaces<S: BuildHasher>(
    advertised: &[[u8; 32]],
    held_caps: &HashSet<[u8; 32], S>,
) -> Vec<[u8; 32]> {
    advertised
        .iter()
        .filter(|ns| held_caps.contains(*ns))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_matching_intersection() {
        let advertised = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
        let held: HashSet<[u8; 32]> = [[2u8; 32], [3u8; 32], [4u8; 32]].into_iter().collect();
        let result = intersect_namespaces(&advertised, &held);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&[2u8; 32]));
        assert!(result.contains(&[3u8; 32]));
    }

    #[test]
    fn namespace_matching_no_overlap() {
        let advertised = vec![[1u8; 32], [2u8; 32]];
        let held: HashSet<[u8; 32]> = [[3u8; 32], [4u8; 32]].into_iter().collect();
        let result = intersect_namespaces(&advertised, &held);
        assert!(result.is_empty());
    }

    #[test]
    fn namespace_matching_full_overlap() {
        let advertised = vec![[1u8; 32], [2u8; 32]];
        let held: HashSet<[u8; 32]> = [[1u8; 32], [2u8; 32]].into_iter().collect();
        let result = intersect_namespaces(&advertised, &held);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn namespace_matching_empty_advertised() {
        let held: HashSet<[u8; 32]> = [[1u8; 32]].into_iter().collect();
        let result = intersect_namespaces(&[], &held);
        assert!(result.is_empty());
    }

    #[test]
    fn namespace_matching_empty_held() {
        let advertised = vec![[1u8; 32]];
        let held: HashSet<[u8; 32]> = HashSet::new();
        let result = intersect_namespaces(&advertised, &held);
        assert!(result.is_empty());
    }
}
