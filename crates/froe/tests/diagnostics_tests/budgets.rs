//! What a hostile store cannot make a diagnostic spend: every ceiling on
//! work, results, pending nodes, and rendered text, checked before the
//! allocation it guards.

use super::*;

#[test]
pub(crate) fn wide_production_tree_hits_typed_result_budget_before_retention_grows_unbounded() {
    let directory = TestDirectory::new("wide-debug-budget");
    write_wide_production_fixture(&directory.path, 128);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let before = directory_snapshot(&directory.path);
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(repository.head_record_identifier().segment))
        .expect("head archive")
        .file_name()
        .to_owned();

    let mut row_options = ArchiveDebugOptions::default();
    row_options.maximum_path_references = 64;
    row_options.maximum_reference_text_bytes = usize::MAX;
    let error = debug_archive_with_options(&repository, &archive_file_name, row_options)
        .expect_err("wide result must stop at the configured limit");
    assert!(matches!(
        error,
        ArchiveDebugError::ResultBudgetExceeded {
            maximum_path_references: 64,
            attempted_path_references: 65,
            ..
        }
    ));

    let mut text_options = ArchiveDebugOptions::default();
    text_options.maximum_path_references = usize::MAX;
    text_options.maximum_reference_text_bytes = 0;
    let text_error = debug_archive_with_options(&repository, &archive_file_name, text_options)
        .expect_err("retained text has an independent limit");
    assert!(matches!(
        text_error,
        ArchiveDebugError::ResultBudgetExceeded {
            maximum_reference_text_bytes: 0,
            attempted_path_references: 1,
            attempted_reference_text_bytes: 1..,
            ..
        }
    ));

    drop(repository);
    assert_eq!(directory_snapshot(&directory.path), before);
    assert!(!directory.path.join("repo.lock").exists());
}

#[test]
pub(crate) fn hostile_reused_block_list_stops_at_the_exact_work_budget() {
    let fixture =
        write_diagnostic_fixture("reused-list-work-budget", GraphFixture::HostileReusedList);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 93;

    let error = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
        .expect_err("the twelfth reused list entry must not be resolved");
    assert!(
        matches!(
            &error,
            ArchiveDebugError::WorkBudgetExceeded {
                maximum_work_units: 93,
                attempted_work_units: 138,
            }
        ),
        "{error:?}"
    );
    assert!(matches!(
        debug_archive(&repository, DATA_ARCHIVE),
        Err(ArchiveDebugError::Repository(
            froe::Error::InvalidFormat { .. }
        ))
    ));
}

#[test]
pub(crate) fn per_node_child_materialization_cap_is_typed_and_checked_before_expansion() {
    let fixture = write_diagnostic_fixture("debug-child-cap", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_scheduled_children_per_node = 0;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeChildBudgetExceeded {
            maximum_scheduled_children_per_node: 0,
            attempted_scheduled_children: 1,
        })
    ));
}

#[test]
pub(crate) fn corrupt_map_cannot_enumerate_past_its_preflighted_child_limit() {
    for (fixture_name, declared_size, leaf_sizes, expected_details) in [
        (
            "debug-corrupt-map-over-count",
            33,
            [17, 17],
            "child map declared 33 entries but enumerated at least 34",
        ),
        (
            "debug-corrupt-map-under-count",
            34,
            [17, 16],
            "child map declared 34 entries but enumerated 33",
        ),
    ] {
        let directory = write_mismatched_child_map_fixture(fixture_name, declared_size, leaf_sizes);
        let repository = Repository::open(&directory.path).expect("open repository");
        for child_limit in [
            ArchiveDebugOptions::default().maximum_scheduled_children_per_node,
            u64::MAX,
        ] {
            let mut options = ArchiveDebugOptions::default();
            options.maximum_scheduled_children_per_node = child_limit;
            let error = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
                .expect_err("the concrete entry mismatch is repository corruption");
            assert!(
                matches!(
                    error,
                    ArchiveDebugError::Repository(froe::Error::InvalidFormat { ref details })
                        if details == expected_details
                ),
                "fixture {fixture_name}, child limit {child_limit}: {error:?}"
            );
        }
    }
}

#[test]
pub(crate) fn archive_work_budget_charges_each_child_map_diff_record_at_an_absolute_threshold() {
    let directory = write_diff_child_map_fixture("debug-map-diff-work");
    let repository = Repository::open(&directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 2;

    // Unit one selects the root traversal step. Only one further unit
    // remains: the first diff fits, but following the second diff attempts
    // absolute unit three before any child-entry materialization.
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 2,
            attempted_work_units: 3,
        })
    ));
}

#[test]
pub(crate) fn archive_combined_scheduling_work_includes_both_child_map_scans() {
    let directory = write_one_child_map_fixture("debug-map-combined-work");
    let repository = Repository::open(&directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_name_bytes_per_node = 5;
    // Unit one selects the root. Scheduling then attempts one child-count
    // unit, one record in the count scan, two records in enumeration, and
    // five name bytes: nine more units. The independent name cap fits; only
    // the combined global budget refuses the absolute tenth unit. Both
    // limits exercise the same exact attempt: seven leaves only six units
    // for scheduling (less than the independent name cap after reserved
    // work), while nine leaves eight.
    for maximum_work_units in [7, 9] {
        options.maximum_work_units = maximum_work_units;
        assert!(matches!(
            debug_archive_with_options(&repository, DATA_ARCHIVE, options),
            Err(ArchiveDebugError::WorkBudgetExceeded {
                maximum_work_units: observed_maximum,
                attempted_work_units: 10,
            }) if observed_maximum == maximum_work_units
        ));
    }
}

#[test]
pub(crate) fn archive_work_budget_charges_template_name_list_lookups_at_the_exact_threshold() {
    let directory = write_template_lookup_work_fixture("debug-template-lookup-work");
    let repository = Repository::open(&directory.path).expect("open repository");
    let complete = debug_archive(&repository, DATA_ARCHIVE).expect("complete report");
    assert_eq!(
        complete.work.consumed_work_units, 220,
        "the independently encoded template/list fixture pins every charged lookup"
    );

    let mut exact = ArchiveDebugOptions::default();
    exact.maximum_work_units = complete.work.consumed_work_units;
    let bounded = debug_archive_with_options(&repository, DATA_ARCHIVE, exact)
        .expect("the exact complete-work limit fits");
    assert_eq!(
        bounded.work.consumed_work_units,
        complete.work.consumed_work_units
    );

    let mut insufficient = exact;
    insufficient.maximum_work_units -= 1;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, insufficient),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 219,
            attempted_work_units: 220,
        })
    ));
}

#[test]
pub(crate) fn deep_wide_tree_hits_total_pending_node_cap() {
    let directory = TestDirectory::new("debug-pending-cap");
    write_deep_wide_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    let mut options = ArchiveDebugOptions::default();
    options.maximum_pending_nodes = 2;

    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, options),
        Err(ArchiveDebugError::PendingNodeBudgetExceeded {
            maximum_pending_nodes: 2,
            attempted_pending_nodes: 3,
        })
    ));
}

#[test]
pub(crate) fn deep_shared_name_paths_charge_each_full_path_copy() {
    let directory = TestDirectory::new("debug-deep-path-work");
    write_deep_shared_name_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 1_127;

    // The fifth nested node's Oak path is five copies of "/shared-name"
    // plus the trailing slash used for its rows: 5 * 12 + 1 = 61 bytes.
    // The independently fixed threshold catches that whole copy as work;
    // charging only the newly scheduled name would not reach 1_177.
    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 1_127,
            attempted_work_units: 1_182,
        })
    ));
}

#[test]
pub(crate) fn hostile_child_name_is_refused_from_its_length_before_materialization() {
    let fixture = write_diagnostic_fixture("debug-child-name-cap", GraphFixture::HostileChildName);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_name_bytes_per_node = 64;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeNameBudgetExceeded {
            maximum_name_bytes_per_node: 64,
            attempted_name_bytes: 16_511,
        })
    ));
}

#[test]
pub(crate) fn hostile_template_property_name_is_refused_before_cache_materialization() {
    let fixture =
        write_diagnostic_fixture("debug-template-name-cap", GraphFixture::HostileTemplateName);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_name_bytes_per_node = 64;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeNameBudgetExceeded {
            maximum_name_bytes_per_node: 64,
            attempted_name_bytes: 16_511,
        })
    ));
}
