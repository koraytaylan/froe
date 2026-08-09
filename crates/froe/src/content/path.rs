//! Content path utilities.
//!
//! Content paths name nodes below the content root: `/` is the root
//! itself, `/content/dam` a descendant. Resolution
//! ([`crate::store::Repository::node_at_path`]) ignores empty names, so
//! `/content//dam/` and `/content/dam` address the same node;
//! [`normalized_path`] renders every such spelling in the one canonical
//! form the rest of the workspace emits.

/// Renders a content path canonically: `/` for the root, otherwise a
/// leading slash and single slashes between names. Empty names produced
/// by duplicate or trailing slashes are dropped, matching how path
/// resolution treats them.
#[must_use]
pub fn normalized_path(path: &str) -> String {
    let names: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
    if names.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", names.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::normalized_path;

    #[test]
    fn normalizes_path_spellings() {
        assert_eq!(normalized_path("/"), "/");
        assert_eq!(normalized_path(""), "/");
        assert_eq!(normalized_path("//"), "/");
        assert_eq!(normalized_path("/content/dam"), "/content/dam");
        assert_eq!(normalized_path("content/dam"), "/content/dam");
        assert_eq!(normalized_path("/content//dam/"), "/content/dam");
    }
}
