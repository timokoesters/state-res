use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    time::SystemTime,
};

use maplit::btreeset;
use ruma::{
    events::EventType,
    identifiers::{EventId, RoomId, RoomVersionId},
};

use crate::{
    error::{Error, Result},
    is_power_event,
    state_event::{Requester, StateEvent},
    state_store::StateStore,
    EventMap, StateMap, StateResolution,
};

/// Resolve sets of state events as they come in. Internally `StateResolution` builds a graph
/// and an auth chain to allow for state conflict resolution.
///
/// ## Arguments
///
/// * `start_index` - Where we are in the sorted event chain. This only makes sense if
/// the library user has kept the state events in order.
///
/// * `state_sets` - The incoming state to resolve. Each `StateMap` represents a possible fork
/// in the state of a room.
///
/// * `event_map` - The `EventMap` acts as a local cache of state, any event that is not found
/// in the `event_map` will be fetched from the `StateStore` and cached in the `event_map`. There
/// is no state kept from separate `resolve` calls, although this could be a potential optimization
/// in the future.
///
/// * `store` - Any type that implements `StateStore` acts as the database. When an event is not
/// found in the `event_map` it will be retrieved from the `store`.
pub fn resolve(
    room_id: &RoomId,
    room_version: &RoomVersionId,
    start_idx: usize,
    state_sets: &[StateMap<EventId>],
    event_map: Option<EventMap<StateEvent>>,
    store: &dyn StateStore,
) -> Result<StateMap<EventId>> {
    tracing::info!("State resolution starting");

    let mut event_map = if let Some(ev_map) = event_map {
        ev_map
    } else {
        EventMap::new()
    };
    // split non-conflicting and conflicting state
    let (clean, conflicting) = StateResolution::separate(&state_sets);

    tracing::info!("non conflicting {:?}", clean.len());

    if conflicting.is_empty() {
        tracing::info!("no conflicting state found");
        return Ok(clean);
    }

    tracing::info!("{} conflicting events", conflicting.len());

    // the set of auth events that are not common across server forks
    let mut auth_diff = StateResolution::get_auth_chain_diff(room_id, &state_sets, store)?;

    tracing::debug!("auth diff size {}", auth_diff.len());

    // add the auth_diff to conflicting now we have a full set of conflicting events
    auth_diff.extend(conflicting.values().cloned().flatten());
    let mut all_conflicted = auth_diff
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    tracing::info!("full conflicted set is {} events", all_conflicted.len());

    // gather missing events for the event_map
    let events = store
        .get_events(
            room_id,
            &all_conflicted
                .iter()
                // we only want the events we don't know about yet
                .filter(|id| !event_map.contains_key(id))
                .cloned()
                .collect::<Vec<_>>(),
        )
        .unwrap();

    // update event_map to include the fetched events
    event_map.extend(events.into_iter().map(|ev| (ev.event_id(), ev)));
    // at this point our event_map == store there should be no missing events

    tracing::debug!("event map size: {}", event_map.len());

    // synapse says `full_set = {eid for eid in full_conflicted_set if eid in event_map}`
    //
    // don't honor events we cannot "verify"
    all_conflicted.retain(|id| event_map.contains_key(id));

    // get only the power events with a state_key: "" or ban/kick event (sender != state_key)
    let power_events = all_conflicted
        .iter()
        .filter(|id| is_power_event(id, &event_map))
        .cloned()
        .collect::<Vec<_>>();

    // sort the power events based on power_level/clock/event_id and outgoing/incoming edges
    let mut sorted_power_levels = StateResolution::reverse_topological_power_sort(
        room_id,
        &power_events,
        &mut event_map,
        store,
        &all_conflicted,
    );

    tracing::debug!(
        "SRTD {:?}",
        sorted_power_levels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    // sequentially auth check each power level event event.
    let resolved = StateResolution::iterative_auth_check(
        room_id,
        room_version,
        &sorted_power_levels,
        &clean,
        &mut event_map,
        store,
    )?;

    tracing::debug!(
        "AUTHED {:?}",
        resolved
            .iter()
            .map(|(key, id)| (key, id.to_string()))
            .collect::<Vec<_>>()
    );

    let events_to_resolve = all_conflicted;

    let power_event = resolved.get(&(EventType::RoomPowerLevels, Some("".into())));

    tracing::debug!("PL {:?}", power_event);

    let sorted_left_events = StateResolution::mainline_sort(
        room_id,
        &events_to_resolve,
        power_event,
        &mut event_map,
        store,
    );

    tracing::debug!(
        "SORTED LEFT {:?}",
        sorted_left_events
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    let mut resolved_state = StateResolution::iterative_auth_check(
        room_id,
        room_version,
        &sorted_left_events,
        &resolved,
        &mut event_map,
        store,
    )?;

    // add unconflicted state to the resolved state
    resolved_state.extend(clean);

    Ok(resolved_state)
}
