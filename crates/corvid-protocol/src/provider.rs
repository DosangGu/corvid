use std::collections::BTreeSet;

use crate::session::valid_id;
use crate::wire::{ProviderAdapter, ProviderProfile};

const MAX_PROFILES: usize = 16;

pub(crate) fn valid_profiles(profiles: &[ProviderProfile]) -> bool {
    let mut identities = BTreeSet::new();
    profiles.len() <= MAX_PROFILES
        && profiles.iter().all(|profile| {
            profile.provider > 0
                && valid_id(&profile.version)
                && identities.insert((profile.provider, profile.version.as_str()))
                && profile.adapter.as_ref().is_some_and(valid_adapter)
        })
}

pub(crate) fn valid_adapter(adapter: &ProviderAdapter) -> bool {
    let mut kinds = BTreeSet::new();
    adapter.mode > 0
        && adapter.schema_revision > 0
        && distinct_positive(&adapter.qualified_events, 32)
        && distinct_positive(&adapter.correlation, 16)
        && adapter.capabilities.len() <= 32
        && adapter.capabilities.iter().all(|capability| {
            capability.kind > 0
                && kinds.insert(capability.kind)
                && capability.qualification > 0
                && capability.reason.len() <= 1024
                && !capability.reason.chars().any(char::is_control)
        })
}

fn distinct_positive(values: &[i32], limit: usize) -> bool {
    let mut seen = BTreeSet::new();
    values.len() <= limit && values.iter().all(|value| *value > 0 && seen.insert(*value))
}
