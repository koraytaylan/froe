//! Downstream reachability checks for bounded content readers and their errors.

#[test]
fn bounded_content_budget_producers_are_publicly_callable() {
    fn call_bounded_readers<'provider>(
        provider: &'provider dyn froe::SegmentProvider,
        identifier: froe::RecordIdentifier,
        traversal: &mut froe::content::DepthFirstTraversal<'provider>,
    ) {
        let _ = froe::content::template::read_template_with_limits(provider, identifier, 1, 1);
        let _ = froe::content::map::map_entries_with_limits(provider, identifier, 1, 1, 1);
        let _ = traversal.next_node_with_scheduling_limits(1, 1, 1, 1);
    }

    fn is_budget_error(error: &froe::Error) -> bool {
        matches!(
            error,
            froe::Error::StringMaterializationBudgetExceeded { .. }
                | froe::Error::TemplatePropertyBudgetExceeded { .. }
                | froe::Error::MapEntryBudgetExceeded { .. }
                | froe::Error::MapTraversalWorkBudgetExceeded { .. }
                | froe::Error::TraversalSchedulingBudgetExceeded { .. }
                | froe::Error::TraversalChildNameBudgetExceeded { .. }
                | froe::Error::TraversalSchedulingWorkBudgetExceeded { .. }
                | froe::Error::TraversalPendingBudgetExceeded { .. }
        )
    }

    let _ = call_bounded_readers;
    let _ = is_budget_error;
}
