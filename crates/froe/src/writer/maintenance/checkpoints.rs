//! Planning checkpoint removal, including the asynchronous references
//! a checkpoint's own subtree still holds.

use super::options::{CompactionOptions, MaintenanceTask};
use super::planning::CheckpointPlan;
use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::error::Result;
use crate::store::Repository;
use std::collections::{BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn plan_checkpoints(
    repository: &Repository,
    options: &CompactionOptions,
    now: SystemTime,
    warnings: &mut Vec<String>,
) -> Result<CheckpointPlan> {
    if !options.contains(MaintenanceTask::ExpiredCheckpoints)
        && !options.contains(MaintenanceTask::UnreferencedCheckpoints)
    {
        return Ok(CheckpointPlan::default());
    }
    let now_milliseconds = now.duration_since(UNIX_EPOCH).map_or(0, |duration| {
        duration.as_millis().min(i64::MAX as u128) as i64
    });
    let checkpoints = repository.checkpoints()?;
    let referenced = if options.contains(MaintenanceTask::UnreferencedCheckpoints) {
        async_checkpoint_references(repository)?
    } else {
        HashSet::new()
    };
    let mut expired_names = BTreeSet::new();
    let mut unreferenced_names = BTreeSet::new();
    for (name, checkpoint) in checkpoints {
        if options.contains(MaintenanceTask::ExpiredCheckpoints) {
            match checkpoint.property("timestamp")? {
                Some(property) => match property.values {
                    PropertyValues::Single(PropertyValue::Long(timestamp)) => {
                        if now_milliseconds > timestamp {
                            expired_names.insert(name.clone());
                        }
                    }
                    _ => warnings.push(format!(
                        "checkpoint {name} has a malformed timestamp and was not selected by expiry"
                    )),
                },
                None => warnings.push(format!(
                    "checkpoint {name} has no timestamp and was not selected by expiry"
                )),
            }
        }
        if options.contains(MaintenanceTask::UnreferencedCheckpoints)
            && !referenced.contains(&name)
            && !expired_names.contains(&name)
        {
            unreferenced_names.insert(name);
        }
    }
    let expired = expired_names.len();
    let unreferenced = unreferenced_names.len();
    expired_names.extend(unreferenced_names);
    Ok(CheckpointPlan {
        names: expired_names.into_iter().collect(),
        expired,
        unreferenced,
    })
}

pub(super) fn async_checkpoint_references(repository: &Repository) -> Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    if let Some(async_state) = repository.content_root()?.child_node(":async")? {
        for property in async_state.properties()? {
            match property.values {
                PropertyValues::Single(PropertyValue::String(value)) => {
                    referenced.insert(value);
                }
                PropertyValues::Multiple(values) => {
                    referenced.extend(values.into_iter().filter_map(|value| match value {
                        PropertyValue::String(value) => Some(value),
                        _ => None,
                    }));
                }
                PropertyValues::Single(_) => {}
            }
        }
    }
    Ok(referenced)
}
