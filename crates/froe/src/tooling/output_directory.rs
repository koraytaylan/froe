//! Where a command may write a directory, and where it may not.
//!
//! One rule, shared: an output directory must not be inside the repository.
//! A stray entry in a segment store is a surprise the next administrator has
//! to investigate, and the open path would have to decide whether it is
//! damage.
//!
//! It lives here rather than in `froe-export`, where it was written, because
//! `froe index dump` needs the same rule and `froe-export` depends on
//! `froe` — a call the other way is a cycle cargo refuses.
//!
//! The file-case rule in `froe_export::output_file::create_export_output`
//! deliberately stays where it is. It canonicalizes the *parent* only and
//! names a file rather than a directory, and each form has its own landed
//! test; the duplication is recorded at both sites rather than resolved by
//! forcing one shape onto both.

use std::path::Path;

use crate::error::{Error, Result};

/// Refuses `directory` when it resolves inside `repository_path`.
///
/// Creates nothing: the caller decides whether the directory should exist
/// and makes it. An already existing directory outside the repository is
/// fine — `froe export` and its refresh both depend on that.
///
/// The directory may not exist yet, and only existing paths canonicalize, so
/// the check runs against the nearest existing ancestor. That is what stops
/// a symlink on the way in from smuggling the target into the repository.
pub fn refuse_output_inside_repository(repository_path: &Path, directory: &Path) -> Result<()> {
    let repository_directory = std::fs::canonicalize(repository_path)?;
    let mut existing_ancestor = directory;
    while !existing_ancestor.exists() {
        existing_ancestor = match existing_ancestor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
    }
    if std::fs::canonicalize(existing_ancestor)?.starts_with(&repository_directory) {
        return Err(Error::InvalidFormat {
            details: format!(
                "output directory {} is inside the repository directory; a stray entry \
                 there could be mistaken for damage at the next open",
                directory.display()
            ),
        });
    }
    Ok(())
}
