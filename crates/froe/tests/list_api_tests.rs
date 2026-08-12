//! Public source-compatibility checks for list access.

use froe::content::list::uncounted_list_entry;
use froe::store::ArchiveSet;
use froe::{RecordIdentifier, Repository};

#[test]
fn uncounted_list_entry_function_item_accepts_multiple_provider_types() {
    fn lookup_from_two_provider_types(
        repository: &Repository,
        archive_set: &ArchiveSet,
        identifier: RecordIdentifier,
    ) {
        let lookup = uncounted_list_entry;
        let _repository_result = lookup(repository, identifier, 1, 0);
        let _archive_result = lookup(archive_set, identifier, 1, 0);
    }

    let _ = lookup_from_two_provider_types;
}
