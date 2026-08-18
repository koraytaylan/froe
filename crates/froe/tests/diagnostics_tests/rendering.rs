//! Rendering a property the way Oak does, across the array and long-value
//! cutoffs where the two could disagree.

use super::*;

#[test]
pub(crate) fn duplicate_rendered_property_lines_use_oak_tree_set_semantics() {
    let directory = write_duplicate_property_fixture("duplicate-property-lines");
    let repository = Repository::open(&directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let duplicate_rows = report
        .references
        .iter()
        .filter(|reference| matches!(reference, ArchivePathReference::Property { name, .. } if name == "dup"))
        .count();

    assert_eq!(duplicate_rows, 1);
    assert_eq!(
        report.work.retained_path_references,
        report.references.len() as u64,
        "only unique rows enter the result ledger"
    );
    let mut exact_unique = ArchiveDebugOptions::default();
    exact_unique.maximum_path_references = report.references.len();
    exact_unique.maximum_reference_text_bytes = report.work.retained_reference_text_bytes as usize;
    let bounded = debug_archive_with_options(&repository, DATA_ARCHIVE, exact_unique)
        .expect("duplicate candidates do not consume aggregate result budget");
    assert_eq!(bounded.references, report.references);
}

#[test]
pub(crate) fn non_string_values_render_fully_across_old_array_and_long_value_cutoffs() {
    for array_size in [1_024usize, 1_025] {
        let directory = TestDirectory::new(&format!("debug-rendering-{array_size}"));
        write_rendering_production_fixture(&directory.path, array_size);
        std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
        let repository = Repository::open(&directory.path).expect("open repository");
        let archive_file_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(repository.head_record_identifier().segment))
            .expect("head archive")
            .file_name()
            .to_owned();
        let report = debug_archive(&repository, &archive_file_name).expect("debug report");
        let display = |name: &str| {
            report
                .references
                .iter()
                .find_map(|reference| match reference {
                    ArchivePathReference::Property {
                        name: property_name,
                        display: ArchivePropertyDisplay::Other(text),
                        ..
                    } if property_name == name => Some(text.as_str()),
                    _ => None,
                })
        };

        let numbers = display("numbers").expect("number array display");
        let expected_numbers = format!("[{}]", vec!["7"; array_size].join(", "));
        assert_eq!(numbers, expected_numbers, "{array_size}-element boundary");
        assert!(!numbers.contains("omitted"));
        assert_eq!(display("minimumDouble"), Some("4.9E-324"));
        assert_eq!(display("minimumDoubles"), Some("[4.9E-324, 4.9E-324]"));
        let expected_long_name = "n".repeat(16_512);
        assert_eq!(display("longName"), Some(expected_long_name.as_str()));

        if array_size == 1_025 {
            let mut options = ArchiveDebugOptions::default();
            options.maximum_reference_text_bytes = 1_024;
            assert!(matches!(
                debug_archive_with_options(&repository, &archive_file_name, options),
                Err(ArchiveDebugError::ResultBudgetExceeded { .. })
            ));
        }
    }
}

#[test]
pub(crate) fn independent_records_pin_full_1025_array_and_minimum_double_spelling() {
    let directory = write_independent_rendering_fixture("independent-debug-rendering");
    let repository = Repository::open(&directory.path).expect("open independent repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let display = |name: &str| {
        report
            .references
            .iter()
            .find_map(|reference| match reference {
                ArchivePathReference::Property {
                    name: property_name,
                    display: ArchivePropertyDisplay::Other(text),
                    ..
                } if property_name == name => Some(text.as_str()),
                _ => None,
            })
    };

    let expected_numbers = format!("[{}]", vec!["7"; 1_025].join(", "));
    assert_eq!(display("numbers"), Some(expected_numbers.as_str()));
    assert_eq!(display("minimumDouble"), Some("4.9E-324"));
    assert_eq!(display("minimumDoubles"), Some("[4.9E-324, 4.9E-324]"));
}

#[test]
pub(crate) fn long_non_string_scalar_is_complete_or_a_typed_text_budget_error() {
    let directory = TestDirectory::new("long-scalar-debug-budget");
    write_long_scalar_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();

    let mut sufficient = ArchiveDebugOptions::default();
    sufficient.maximum_reference_text_bytes = 20_000;
    let report = debug_archive_with_options(&repository, &archive_file_name, sufficient)
        .expect("the configured report budget holds the complete scalar");
    let expected = "n".repeat(16_512);
    assert!(report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Property {
            name,
            display: ArchivePropertyDisplay::Other(display),
            ..
        } if name == "longName" && display == &expected
    )));

    let mut insufficient = ArchiveDebugOptions::default();
    insufficient.maximum_reference_text_bytes = 1_024;
    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, insufficient),
        Err(ArchiveDebugError::ResultBudgetExceeded {
            maximum_reference_text_bytes: 1_024,
            attempted_reference_text_bytes: 1_025,
            ..
        })
    ));
}

#[test]
pub(crate) fn unavailable_and_corrupt_binary_scalars_render_oak_negative_size() {
    let directory = write_external_binary_fixture("external-debug-display");
    let repository = Repository::open(&directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let displays: Vec<(&str, &str)> = report
        .references
        .iter()
        .filter_map(|reference| match reference {
            ArchivePathReference::Property {
                name,
                display: ArchivePropertyDisplay::Other(display),
                ..
            } if name.ends_with("External") || name == "corruptBinary" => {
                Some((name.as_str(), display.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        displays,
        [
            ("corruptBinary", "{-1 bytes}"),
            ("longExternal", "{-1 bytes}"),
            ("shortExternal", "{-1 bytes}"),
        ]
    );
}
