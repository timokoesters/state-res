#![allow(clippy::or_fun_call, clippy::expect_fun_call)]

use std::{collections::BTreeMap, convert::TryFrom, sync::Once, time::UNIX_EPOCH, sync::Arc};

use ruma::{
    events::{
        pdu::EventHash,
        room::{
            join_rules::JoinRule,
            member::{MemberEventContent, MembershipState},
        },
        EventType,
    },
    identifiers::{EventId, RoomId, RoomVersionId, UserId},
};
use serde_json::{json, Value as JsonValue};
use state_res::{Error, Result, StateEvent, StateMap, StateResolution, StateStore};
use tracing_subscriber as tracer;

static LOGGER: Once = Once::new();

static mut SERVER_TIMESTAMP: i32 = 0;

fn do_check(events: &[Arc<StateEvent>], edges: Vec<Vec<EventId>>, expected_state_ids: Vec<EventId>) {
    // to activate logging use `RUST_LOG=debug cargo t`
    let _ = LOGGER.call_once(|| {
        tracer::fmt()
            .with_env_filter(tracer::EnvFilter::from_default_env())
            .init()
    });

    let mut store = TestStore(
        INITIAL_EVENTS()
            .values()
            .chain(events)
            .map(|ev| (ev.event_id().clone(), ev.clone()))
            .collect(),
    );

    // This will be lexi_topo_sorted for resolution
    let mut graph = BTreeMap::new();
    // this is the same as in `resolve` event_id -> StateEvent
    let mut fake_event_map = BTreeMap::new();

    // create the DB of events that led up to this point
    // TODO maybe clean up some of these clones it is just tests but...
    for ev in INITIAL_EVENTS().values().chain(events) {
        graph.insert(ev.event_id().clone(), vec![]);
        fake_event_map.insert(ev.event_id().clone(), ev.clone());
    }

    for pair in INITIAL_EDGES().windows(2) {
        if let [a, b] = &pair {
            graph.entry(a.clone()).or_insert(vec![]).push(b.clone());
        }
    }

    for edge_list in edges {
        for pair in edge_list.windows(2) {
            if let [a, b] = &pair {
                graph.entry(a.clone()).or_insert(vec![]).push(b.clone());
            }
        }
    }

    // event_id -> StateEvent
    let mut event_map: BTreeMap<EventId, Arc<StateEvent>> = BTreeMap::new();
    // event_id -> StateMap<EventId>
    let mut state_at_event: BTreeMap<EventId, StateMap<EventId>> = BTreeMap::new();

    // resolve the current state and add it to the state_at_event map then continue
    // on in "time"
    for node in
        StateResolution::lexicographical_topological_sort(&graph, |id| (0, UNIX_EPOCH, id.clone()))
    {
        let fake_event = fake_event_map.get(&node).unwrap();
        let event_id = fake_event.event_id();

        let prev_events = graph.get(&node).unwrap();

        let state_before: StateMap<EventId> = if prev_events.is_empty() {
            BTreeMap::new()
        } else if prev_events.len() == 1 {
            state_at_event.get(&prev_events[0]).unwrap().clone()
        } else {
            let state_sets = prev_events
                .iter()
                .filter_map(|k| state_at_event.get(k))
                .cloned()
                .collect::<Vec<_>>();

            tracing::info!(
                "{:#?}",
                state_sets
                    .iter()
                    .map(|map| map
                        .iter()
                        .map(|((ty, key), id)| format!("(({}{:?}), {})", ty, key, id))
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            );

            let resolved = StateResolution::resolve(
                &room_id(),
                &RoomVersionId::Version1,
                &state_sets,
                Some(event_map.clone()),
                &store,
            );
            match resolved {
                Ok(state) => state,
                Err(e) => panic!("resolution for {} failed: {}", node, e),
            }
        };

        let mut state_after = state_before.clone();

        let ty = fake_event.kind().clone();
        let key = fake_event.state_key().clone();
        state_after.insert((ty, key), event_id.clone());

        let auth_types = state_res::auth_types_for_event(
            fake_event.kind(),
            fake_event.sender(),
            Some(fake_event.state_key()),
            fake_event.content().clone(),
        );

        let mut auth_events = vec![];
        for key in auth_types {
            if state_before.contains_key(&key) {
                auth_events.push(state_before[&key].clone())
            }
        }

        // TODO The event is just remade, adding the auth_events and prev_events here
        // the `to_pdu_event` was split into `init` and the fn below, could be better
        let e = fake_event;
        let ev_id = e.event_id();
        let event = to_pdu_event(
            &e.event_id().to_string(),
            e.sender().clone(),
            e.kind(),
            Some(&e.state_key()),
            e.content().clone(),
            &auth_events,
            prev_events,
        );

        // we have to update our store, an actual user of this lib would
        // be giving us state from a DB.
        store.0.insert(ev_id.clone(), event.clone());

        state_at_event.insert(node, state_after);
        event_map.insert(event_id.clone(), Arc::clone(store.0.get(&ev_id).unwrap()));
    }

    let mut expected_state = StateMap::new();
    for node in expected_state_ids {
        let ev = event_map.get(&node).expect(&format!(
            "{} not found in {:?}",
            node.to_string(),
            event_map
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        ));

        let key = (ev.kind(), ev.state_key());

        expected_state.insert(key, node);
    }

    let start_state = state_at_event.get(&event_id("$START:foo")).unwrap();

    let end_state = state_at_event
        .get(&event_id("$END:foo"))
        .unwrap()
        .iter()
        .filter(|(k, v)| expected_state.contains_key(k) || start_state.get(k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<StateMap<EventId>>();

    assert_eq!(expected_state, end_state);
}
pub struct TestStore(BTreeMap<EventId, Arc<StateEvent>>);

#[allow(unused)]
impl StateStore for TestStore {
    fn get_event(&self, room_id: &RoomId, event_id: &EventId) -> Result<Arc<StateEvent>> {
        self.0
            .get(event_id)
            .map(Arc::clone)
            .ok_or_else(|| Error::NotFound(format!("{} not found", event_id.to_string())))
    }
}

fn event_id(id: &str) -> EventId {
    if id.contains('$') {
        return EventId::try_from(id).unwrap();
    }
    EventId::try_from(format!("${}:foo", id)).unwrap()
}

fn alice() -> UserId {
    UserId::try_from("@alice:foo").unwrap()
}
fn bob() -> UserId {
    UserId::try_from("@bob:foo").unwrap()
}
fn charlie() -> UserId {
    UserId::try_from("@charlie:foo").unwrap()
}
fn ella() -> UserId {
    UserId::try_from("@ella:foo").unwrap()
}
fn zara() -> UserId {
    UserId::try_from("@zara:foo").unwrap()
}

fn room_id() -> RoomId {
    RoomId::try_from("!test:foo").unwrap()
}

fn member_content_ban() -> JsonValue {
    serde_json::to_value(MemberEventContent {
        membership: MembershipState::Ban,
        displayname: None,
        avatar_url: None,
        is_direct: None,
        third_party_invite: None,
    })
    .unwrap()
}

fn member_content_join() -> JsonValue {
    serde_json::to_value(MemberEventContent {
        membership: MembershipState::Join,
        displayname: None,
        avatar_url: None,
        is_direct: None,
        third_party_invite: None,
    })
    .unwrap()
}

fn to_pdu_event<S>(
    id: &str,
    sender: UserId,
    ev_type: EventType,
    state_key: Option<&str>,
    content: JsonValue,
    auth_events: &[S],
    prev_events: &[S],
) -> Arc<StateEvent>
where
    S: AsRef<str>,
{
    let ts = unsafe {
        let ts = SERVER_TIMESTAMP;
        // increment the "origin_server_ts" value
        SERVER_TIMESTAMP += 1;
        ts
    };
    let id = if id.contains('$') {
        id.to_string()
    } else {
        format!("${}:foo", id)
    };
    let auth_events = auth_events
        .iter()
        .map(AsRef::as_ref)
        .map(event_id)
        .map(|id| {
            (
                id,
                EventHash {
                    sha256: "hello".into(),
                },
            )
        })
        .collect::<Vec<_>>();
    let prev_events = prev_events
        .iter()
        .map(AsRef::as_ref)
        .map(event_id)
        .map(|id| {
            (
                id,
                EventHash {
                    sha256: "hello".into(),
                },
            )
        })
        .collect::<Vec<_>>();

    let json = if let Some(state_key) = state_key {
        json!({
            "auth_events": auth_events,
            "prev_events": prev_events,
            "event_id": id,
            "sender": sender,
            "type": ev_type,
            "state_key": state_key,
            "content": content,
            "origin_server_ts": ts,
            "room_id": room_id(),
            "origin": "foo",
            "depth": 0,
            "hashes": { "sha256": "hello" },
            "signatures": {},
        })
    } else {
        json!({
            "auth_events": auth_events,
            "prev_events": prev_events,
            "event_id": id,
            "sender": sender,
            "type": ev_type,
            "content": content,
            "origin_server_ts": ts,
            "room_id": room_id(),
            "origin": "foo",
            "depth": 0,
            "hashes": { "sha256": "hello" },
            "signatures": {},
        })
    };
    Arc::new(serde_json::from_value(json).unwrap())
}

// all graphs start with these input events
#[allow(non_snake_case)]
fn INITIAL_EVENTS() -> BTreeMap<EventId, Arc<StateEvent>> {
    // this is always called so we can init the logger here
    let _ = LOGGER.call_once(|| {
        tracer::fmt()
            .with_env_filter(tracer::EnvFilter::from_default_env())
            .init()
    });

    vec![
        to_pdu_event::<EventId>(
            "CREATE",
            alice(),
            EventType::RoomCreate,
            Some(""),
            json!({ "creator": alice() }),
            &[],
            &[],
        ),
        to_pdu_event(
            "IMA",
            alice(),
            EventType::RoomMember,
            Some(alice().to_string().as_str()),
            member_content_join(),
            &["CREATE"],
            &["CREATE"],
        ),
        to_pdu_event(
            "IPOWER",
            alice(),
            EventType::RoomPowerLevels,
            Some(""),
            json!({"users": {alice().to_string(): 100}}),
            &["CREATE", "IMA"],
            &["IMA"],
        ),
        to_pdu_event(
            "IJR",
            alice(),
            EventType::RoomJoinRules,
            Some(""),
            json!({ "join_rule": JoinRule::Public }),
            &["CREATE", "IMA", "IPOWER"],
            &["IPOWER"],
        ),
        to_pdu_event(
            "IMB",
            bob(),
            EventType::RoomMember,
            Some(bob().to_string().as_str()),
            member_content_join(),
            &["CREATE", "IJR", "IPOWER"],
            &["IJR"],
        ),
        to_pdu_event(
            "IMC",
            charlie(),
            EventType::RoomMember,
            Some(charlie().to_string().as_str()),
            member_content_join(),
            &["CREATE", "IJR", "IPOWER"],
            &["IMB"],
        ),
        to_pdu_event::<EventId>(
            "START",
            charlie(),
            EventType::RoomMessage,
            None,
            json!({}),
            &[],
            &[],
        ),
        to_pdu_event::<EventId>(
            "END",
            charlie(),
            EventType::RoomMessage,
            None,
            json!({}),
            &[],
            &[],
        ),
    ]
    .into_iter()
    .map(|ev| (ev.event_id().clone(), ev))
    .collect()
}

#[allow(non_snake_case)]
fn INITIAL_EDGES() -> Vec<EventId> {
    vec!["START", "IMC", "IMB", "IJR", "IPOWER", "IMA", "CREATE"]
        .into_iter()
        .map(event_id)
        .collect::<Vec<_>>()
}

// all graphs start with these input events
#[allow(non_snake_case)]
fn BAN_STATE_SET() -> BTreeMap<EventId, Arc<StateEvent>> {
    vec![
        to_pdu_event(
            "PA",
            alice(),
            EventType::RoomPowerLevels,
            Some(""),
            json!({"users": {alice(): 100, bob(): 50}}),
            &["CREATE", "IMA", "IPOWER"], // auth_events
            &["START"],                   // prev_events
        ),
        to_pdu_event(
            "PB",
            alice(),
            EventType::RoomPowerLevels,
            Some(""),
            json!({"users": {alice(): 100, bob(): 50}}),
            &["CREATE", "IMA", "IPOWER"],
            &["END"],
        ),
        to_pdu_event(
            "MB",
            alice(),
            EventType::RoomMember,
            Some(ella().as_str()),
            member_content_ban(),
            &["CREATE", "IMA", "PB"],
            &["PA"],
        ),
        to_pdu_event(
            "IME",
            ella(),
            EventType::RoomMember,
            Some(ella().as_str()),
            member_content_join(),
            &["CREATE", "IJR", "PA"],
            &["MB"],
        ),
    ]
    .into_iter()
    .map(|ev| (ev.event_id().clone(), ev))
    .collect()
}

#[test]
fn ban_with_auth_chains() {
    let ban = BAN_STATE_SET();

    let edges = vec![vec!["END", "MB", "PA", "START"], vec!["END", "IME", "MB"]]
        .into_iter()
        .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
        .collect::<Vec<_>>();

    let expected_state_ids = vec!["PA", "MB"]
        .into_iter()
        .map(event_id)
        .collect::<Vec<_>>();

    do_check(
        &ban.values().cloned().collect::<Vec<_>>(),
        edges,
        expected_state_ids,
    );
}

#[test]
fn base_with_auth_chains() {
    let store = TestStore(INITIAL_EVENTS());

    let resolved: BTreeMap<_, EventId> =
        match StateResolution::resolve(&room_id(), &RoomVersionId::Version2, &[], None, &store) {
            Ok(state) => state,
            Err(e) => panic!("{}", e),
        };

    let resolved = resolved
        .values()
        .cloned()
        .chain(INITIAL_EVENTS().values().map(|e| e.event_id().clone()))
        .collect::<Vec<_>>();

    let expected = vec![
        "$CREATE:foo",
        "$IJR:foo",
        "$IPOWER:foo",
        "$IMA:foo",
        "$IMB:foo",
        "$IMC:foo",
        "START",
        "END",
    ];
    for id in expected.iter().map(|i| event_id(i)) {
        // make sure our resolved events are equall to the expected list
        assert!(resolved.iter().any(|eid| eid == &id), "{}", id)
    }
    assert_eq!(expected.len(), resolved.len())
}

#[test]
fn ban_with_auth_chains2() {
    let init = INITIAL_EVENTS();
    let ban = BAN_STATE_SET();

    let mut inner = init.clone();
    inner.extend(ban);
    let store = TestStore(inner.clone());

    let state_set_a = [
        inner.get(&event_id("CREATE")).unwrap(),
        inner.get(&event_id("IJR")).unwrap(),
        inner.get(&event_id("IMA")).unwrap(),
        inner.get(&event_id("IMB")).unwrap(),
        inner.get(&event_id("IMC")).unwrap(),
        inner.get(&event_id("MB")).unwrap(),
        inner.get(&event_id("PA")).unwrap(),
    ]
    .iter()
    .map(|ev| ((ev.kind(), ev.state_key()), ev.event_id().clone()))
    .collect::<BTreeMap<_, _>>();

    let state_set_b = [
        inner.get(&event_id("CREATE")).unwrap(),
        inner.get(&event_id("IJR")).unwrap(),
        inner.get(&event_id("IMA")).unwrap(),
        inner.get(&event_id("IMB")).unwrap(),
        inner.get(&event_id("IMC")).unwrap(),
        inner.get(&event_id("IME")).unwrap(),
        inner.get(&event_id("PA")).unwrap(),
    ]
    .iter()
    .map(|ev| ((ev.kind(), ev.state_key()), ev.event_id().clone()))
    .collect::<StateMap<_>>();

    let resolved: StateMap<EventId> = match StateResolution::resolve(
        &room_id(),
        &RoomVersionId::Version2,
        &[state_set_a, state_set_b],
        None,
        &store,
    ) {
        Ok(state) => state,
        Err(e) => panic!("{}", e),
    };

    tracing::debug!(
        "{:#?}",
        resolved
            .iter()
            .map(|((ty, key), id)| format!("(({}{:?}), {})", ty, key, id))
            .collect::<Vec<_>>()
    );

    let expected = vec![
        "$CREATE:foo",
        "$IJR:foo",
        "$PA:foo",
        "$IMA:foo",
        "$IMB:foo",
        "$IMC:foo",
        "$MB:foo",
    ];

    for id in expected.iter().map(|i| event_id(i)) {
        // make sure our resolved events are equall to the expected list
        assert!(
            resolved.values().any(|eid| eid == &id) || init.contains_key(&id),
            "{}",
            id
        )
    }
    assert_eq!(expected.len(), resolved.len())
}

// all graphs start with these input events
#[allow(non_snake_case)]
fn JOIN_RULE() -> BTreeMap<EventId, Arc<StateEvent>> {
    vec![
        to_pdu_event(
            "JR",
            alice(),
            EventType::RoomJoinRules,
            Some(""),
            json!({"join_rule": "invite"}),
            &["CREATE", "IMA", "IPOWER"],
            &["START"],
        ),
        to_pdu_event(
            "IMZ",
            zara(),
            EventType::RoomPowerLevels,
            Some(zara().as_str()),
            member_content_join(),
            &["CREATE", "JR", "IPOWER"],
            &["START"],
        ),
    ]
    .into_iter()
    .map(|ev| (ev.event_id().clone(), ev))
    .collect()
}

#[test]
fn join_rule_with_auth_chain() {
    let join_rule = JOIN_RULE();

    let edges = vec![vec!["END", "JR", "START"], vec!["END", "IMZ", "START"]]
        .into_iter()
        .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
        .collect::<Vec<_>>();

    let expected_state_ids = vec!["JR"].into_iter().map(event_id).collect::<Vec<_>>();

    do_check(
        &join_rule.values().cloned().collect::<Vec<_>>(),
        edges,
        expected_state_ids,
    );
}
