use std::{collections::BTreeSet, ops::Deref};

use ruma::{
    events::EventType,
    identifiers::{EventId, RoomId, RoomVersionId},
};

use crate::{
    error::Result, is_power_event, state_event::StateEvent, state_store::StateStore, EventMap,
    StateMap, StateResolution,
};

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum OwnState<T> {
    Clean(T),
    Add(T),
    Replaced { remove: T, with: T },
    Remove(T),
    NotResolved(T),
}

impl<T> Deref for OwnState<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            OwnState::Clean(id)
            | OwnState::NotResolved(id)
            | OwnState::Replaced { remove: id, .. }
            | OwnState::Add(id) // TODO will this work ?
            | OwnState::Remove(id) => id,
        }
    }
}

/// Resolve sets of state events as they come in. Internally `StateResolution` builds a graph
/// and an auth chain to allow for state conflict resolution.
///
/// ## Arguments
///
/// * `own_state` - If we track the delta between the state from our server and the resolved state
/// we know the DB operations to rectify it.
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
    own_state: &[EventId],
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

    // Mark the clean state as such
    let mut own_state = own_state
        .iter()
        .map(|id| {
            if clean.values().any(|cid| id == cid) {
                OwnState::Clean(id.clone())
            } else {
                OwnState::NotResolved(id.clone())
            }
        })
        .collect::<Vec<_>>();

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

    // TODO check if we actually need to gather these events from the DB
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
    let sorted_power_levels = StateResolution::reverse_topological_power_sort(
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

    // Mark the resolved power_events as clean
    for id in own_state.iter_mut() {
        if resolved.values().any(|res_id| (&*id).deref() == res_id) {
            *id = OwnState::Clean((&*id).deref().clone());
        }
    }

    // Now check that there are no additional events in the power_events
    for p_event in resolved.values().filter_map(|id| event_map.get(id)) {
        if let Some(idx) = own_state
            .iter()
            .position(|id| (&*id).deref() == &p_event.event_id())
        {
            // TODO This case should never happen because of the above loop right?
            if let OwnState::NotResolved(id) = &mut own_state[idx] {
                own_state[idx] = OwnState::Clean(id.clone())
            }
        // This is the check we need
        } else {
            for prev in p_event.prev_event_ids() {
                if let Some(idx) = own_state.iter().position(|id| (&*id).deref() == &prev) {
                    own_state.insert(idx, OwnState::Add(p_event.event_id()));
                    break;
                } else {
                    todo!("found unconnected state event");
                }
            }
        }
    }

    tracing::debug!(
        "AUTHED {:?}",
        resolved
            .iter()
            .map(|(key, id)| (key, id.to_string()))
            .collect::<Vec<_>>()
    );

    // Normally the resolved power events are filtered out as they don't need to
    // be authed again, since we want the whole state we sort everything
    let events_to_resolve = all_conflicted;

    let power_event = resolved.get(&(EventType::RoomPowerLevels, Some("".into())));

    tracing::debug!("PL {:?}", power_event);

    let sorted_events = StateResolution::mainline_sort(
        room_id,
        &events_to_resolve,
        power_event,
        &mut event_map,
        store,
    );

    tracing::debug!(
        "SORTED LEFT {:?}",
        sorted_events
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    let mut resolved_state = StateResolution::iterative_auth_check(
        room_id,
        room_version,
        &sorted_events,
        &resolved,
        &mut event_map,
        store,
    )?;

    for id in own_state.iter_mut() {
        if resolved.values().any(|res_id| (&*id).deref() == res_id) {
            *id = OwnState::Clean((&*id).deref().clone());
        } else {
            // TODO this stays ? (`let events_to_resolve = all_conflicted;`) ~40 lines up
            // we may still be able to filter the power_events out so the following is valid
            // all events have to known at this time for this to work
            *id = OwnState::Remove((&*id).deref().clone());
        }
    }

    // Now check that there are no additional events in the power_events
    for res_event in resolved_state.values().filter_map(|id| event_map.get(id)) {
        if let Some(idx) = own_state
            .iter()
            .position(|id| (&*id).deref() == &res_event.event_id())
        {
            // TODO This case should never happen because of the above loop right?
            if let OwnState::NotResolved(id) = &mut own_state[idx] {
                own_state[idx] = OwnState::Clean(id.clone())
            }
        // This is the check we need
        } else {
            for prev in res_event.prev_event_ids() {
                if let Some(idx) = own_state.iter().position(|id| (&*id).deref() == &prev) {
                    own_state.insert(idx, OwnState::Add(res_event.event_id()));
                    break;
                } else {
                    todo!("we have a completely unconnected state event")
                }
            }
        }
    }

    for id in &own_state {
        assert!(!matches!(id, OwnState::NotResolved(_)))
    }

    // add unconflicted state to the resolved state
    resolved_state.extend(clean);

    Ok(resolved_state)
}
