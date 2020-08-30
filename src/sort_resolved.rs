use std::{collections::BTreeSet, ops::Deref};

use ruma::{
    events::EventType,
    identifiers::{EventId, RoomId, RoomVersionId},
};

use crate::{
    error::Result, is_power_event, state_event::StateEvent, state_store::StateStore, EventMap,
    StateMap, StateResolution,
};

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq)]
pub enum OwnState<T> {
    /// This eventId is found in both the `own_state` and the resolved state.
    Clean(T),
    /// This eventId needs to be added to to the DAG.
    // TODO this inserts itself after the most recent prev_event that `own_state` knows.
    // make this more robust
    Add(T),
    /// Remove `replace` and insert `with` in it's spot.
    Replaced { replace: T, with: T },
    /// So the sorting of `Replaced` eventId's is kept the `with` ID must be removed.
    Used(T),
    /// This is not a valid event, remove it from the DAG.
    Remove(T),
    /// This event was not handled. Any remaining `NotResolved` variants should
    /// be considered an error, I think `:p`.
    NotResolved(T),
}

impl std::fmt::Debug for OwnState<EventId> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnState::Add(id) => f.debug_tuple("OwnState::Add").field(&id.as_str()).finish(),
            OwnState::Clean(id) => f
                .debug_tuple("OwnState::Clean")
                .field(&id.as_str())
                .finish(),
            OwnState::Replaced { replace, with } => f
                .debug_tuple("OwnState::Replace")
                .field(&replace.as_str())
                .field(&with.as_str())
                .finish(),
            OwnState::Remove(id) => f
                .debug_tuple("OwnState::Remove")
                .field(&id.as_str())
                .finish(),
            OwnState::Used(id) => f.debug_tuple("OwnState::Used").field(&id.as_str()).finish(),
            OwnState::NotResolved(id) => f
                .debug_tuple("OwnState::NotResolved")
                .field(&id.as_str())
                .finish(),
        }
    }
}

impl<T> Deref for OwnState<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            OwnState::Clean(id)
            | OwnState::NotResolved(id)
            | OwnState::Replaced { replace: id, .. }
            | OwnState::Add(id)
            | OwnState::Used(id)
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

    // get only the control events with a state_key: "" or ban/kick event (sender != state_key)
    let power_events = all_conflicted
        .iter()
        .filter(|id| is_power_event(id, &event_map))
        .cloned()
        .collect::<Vec<_>>();

    // sort the power events based on power_level/clock/event_id and outgoing/incoming edges
    let sorted_control_events = StateResolution::reverse_topological_power_sort(
        room_id,
        &power_events,
        &mut event_map,
        store,
        &all_conflicted,
    );

    tracing::debug!(
        "SRTD {:?}",
        sorted_control_events
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    // sequentially auth check each power level event event.
    let resolved_control_events = StateResolution::iterative_auth_check(
        room_id,
        room_version,
        &sorted_control_events,
        &clean,
        &mut event_map,
        store,
    )?;

    tracing::debug!(
        "AUTHED {:?}",
        resolved_control_events
            .iter()
            .map(|(key, id)| (key, id.to_string()))
            .collect::<Vec<_>>()
    );

    // This removes the control events that passed auth and more importantly those that failed auth
    let events_to_resolve = all_conflicted
        .iter()
        .filter(|id| {
            // remove values that are non-resolved control events
            !sorted_control_events.contains(id)
                || resolved_control_events.values().any(|c_id| *id == c_id)
        })
        .cloned()
        .collect::<Vec<_>>();

    let power_event = resolved_control_events.get(&(EventType::RoomPowerLevels, Some("".into())));

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
        &resolved_control_events,
        &mut event_map,
        store,
    )?;

    let peek_state = own_state.to_vec();

    let mut replaced = vec![];
    for id in own_state.iter_mut() {
        if resolved_state
            .values()
            .any(|res_id| (&*id).deref() == res_id)
        {
            *id = OwnState::Clean((&*id).deref().clone());
        } else {
            if matches!(id, OwnState::Clean(_)) {
                continue;
            }

            if let Some(old) =
                StateResolution::get_or_load_event(room_id, (&*id).deref(), &mut event_map, store)
            {
                let key = (old.kind(), old.state_key());
                if let Some(replacement) = resolved_state.get(&key) {
                    *id = OwnState::Replaced {
                        replace: old.event_id(),
                        with: replacement.clone(),
                    };
                    // mark the replacement as Used so it is not
                    // inserted out of order into the DB
                    if let Some(used_idx) = peek_state
                        .iter()
                        .position(|id| (&*id).deref() == replacement)
                    {
                        replaced.push(used_idx);
                    }
                    continue;
                }
            }
            // TODO this stays ? (`let events_to_resolve = all_conflicted;`) ~40 lines up
            // we may still be able to filter the power_events out so the following is valid
            // all events have to be known at this time for this to work
            *id = OwnState::Remove((&*id).deref().clone());
        }
    }

    for index in replaced {
        let event_id = own_state[index].deref().clone();

        own_state[index] = OwnState::Used(event_id);
    }

    // Now check that there are no additional events in the power_events
    for res_event in resolved_state.values().filter_map(|id| event_map.get(id)) {
        if let Some(idx) = own_state
            .iter()
            .position(|id| (&*id).deref() == &res_event.event_id())
        {
            let own_evid = &own_state[idx];

            // TODO This case should never happen because of the above loop right?
            if let OwnState::NotResolved(id) = own_evid {
                own_state[idx] = OwnState::Clean(id.clone());
            }
        // This is the check we need, if we don't have an event in our state but it
        // exists in the resolved state add it where it belongs
        } else {
            for prev in res_event.prev_event_ids() {
                if let Some(idx) = own_state.iter().position(|id| (&*id).deref() == &prev) {
                    own_state.insert(idx, OwnState::Add(res_event.event_id()));
                    break;
                } else {
                    // TODO no parent event found this means we have a... soft fail and continuing fork?
                    own_state.insert(0, OwnState::Add(res_event.event_id()));
                }
            }
        }
    }

    for id in &own_state {
        assert!(!matches!(id, OwnState::NotResolved(_)))
    }

    println!("{:#?}", own_state);

    // add unconflicted state to the resolved state
    resolved_state.extend(clean);

    Ok(resolved_state)
}
