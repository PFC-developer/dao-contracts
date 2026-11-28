use super::*;

type Fixture = (
    OwnedDeps<MockStorage, MockApi, MockQuerier>,
    Env,
    Rc<RefCell<Host>>,
);

fn push(host: &Rc<RefCell<Host>>, start_height: u64, status: Status) -> u64 {
    let mut host = host.borrow_mut();
    let id = host.proposals.len() as u64 + 1;
    host.proposals.push(ProposalRecord {
        id,
        proposal: snapshot(start_height, status),
    });
    id
}

fn set_status(host: &Rc<RefCell<Host>>, id: u64, status: Status) {
    host.borrow_mut().proposals[id as usize - 1].proposal.status = status;
}

fn set_msgs(host: &Rc<RefCell<Host>>, id: u64, msgs: &[CosmosMsg]) {
    host.borrow_mut().messages.insert(id, msgs.to_vec());
}

fn msgs_of(host: &Rc<RefCell<Host>>, id: u64) -> Vec<CosmosMsg> {
    proposal_msgs(&host.borrow(), id)
}

// Every blocked proposal ID, whichever messages it is filed under.
fn gated(deps: &OwnedDeps<MockStorage, MockApi, MockQuerier>) -> Vec<u64> {
    let mut ids: Vec<u64> = GATED
        .range(&deps.storage, None, None, Order::Ascending)
        .flat_map(|entry| entry.unwrap().1)
        .collect();
    ids.sort_unstable();
    ids
}

fn cursor(deps: &OwnedDeps<MockStorage, MockApi, MockQuerier>) -> u64 {
    BOUNDARY.load(&deps.storage).unwrap().proposal_count
}

fn attribute<'a>(response: &'a Response, key: &str) -> Option<&'a str> {
    response
        .attributes
        .iter()
        .find(|attr| attr.key == key)
        .map(|attr| attr.value.as_str())
}

// Adopt with the given proposals already created after the reviewed one.
fn adopted(after_review: &[Status]) -> (Fixture, Response) {
    let (mut deps, env, host, payload) = fixture(4);
    for status in after_review {
        push(&host, env.block.height - 1, *status);
    }
    let response = migrate(deps.as_mut(), env.clone(), payload).unwrap();
    ((deps, env, host), response)
}

#[test]
fn hook_is_untouched_before_adoption() {
    let (mut deps, env, _, _) = fixture(4);
    let msgs = fixture_msg("early");
    let response = hook(deps.as_mut(), &env, &msgs).unwrap();
    assert_eq!(response, Response::new().add_messages(msgs));
    assert!(gated(&deps).is_empty());
}

#[test]
fn adopted_core_accepts_hooks_only_from_the_pinned_module() {
    let ((mut deps, mut env, _), _) = adopted(&[]);
    env.block.height += 1;
    let other = Addr::unchecked("fixture-other-module");
    PROPOSAL_MODULES
        .save(
            &mut deps.storage,
            other.clone(),
            &ProposalModule {
                address: other.clone(),
                prefix: "B".to_owned(),
                status: ProposalModuleStatus::Enabled,
            },
        )
        .unwrap();
    let response = contract::execute(
        deps.as_mut(),
        env,
        cosmwasm_std::testing::mock_info(other.as_str(), &[]),
        dao_interface::msg::ExecuteMsg::ExecuteProposalHook {
            msgs: fixture_msg("other module"),
        },
    );
    assert!(response
        .unwrap_err()
        .to_string()
        .contains("unexpected proposal hook sender"));
}

#[test]
fn proposal_created_after_review_is_gated_instead_of_blocking_adoption() {
    let ((mut deps, mut env, host), response) = adopted(&[Status::Open]);
    assert_eq!(attribute(&response, "gated_proposals"), Some("2"));
    assert_eq!(gated(&deps), vec![2]);
    assert_eq!(cursor(&deps), 2);

    // A later proposal executes while the gated one is open, and after it passes.
    env.block.height += 1;
    let later = fixture_msg("later");
    let id = push(&host, env.block.height, Status::Executed);
    set_msgs(&host, id, &later);
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    set_status(&host, 2, Status::Passed);
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    assert_eq!(gated(&deps), vec![2]);

    // The gated proposal's own execution is refused.
    let stale = msgs_of(&host, 2);
    set_status(&host, 2, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &stale)
        .unwrap_err()
        .to_string()
        .contains("proposal 2 predates the current membership"));
    assert_eq!(gated(&deps), vec![2]);

    // The module records the failure. Other proposals are unaffected, and a
    // new proposal carrying the same messages executes and drops the entry.
    set_status(&host, 2, Status::ExecutionFailed);
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    assert_eq!(gated(&deps), vec![2]);
    assert!(hook(deps.as_mut(), &env, &stale).is_ok());
    assert!(gated(&deps).is_empty());
    assert!(hook(deps.as_mut(), &env, &stale).is_ok());
}

#[test]
fn adoption_without_unresolved_proposals_reports_none() {
    let ((deps, _, _), response) = adopted(&[Status::Executed, Status::Rejected]);
    assert_eq!(attribute(&response, "gated_proposals"), None);
    assert!(gated(&deps).is_empty());
    assert_eq!(cursor(&deps), 3);
}

#[test]
fn settled_gated_proposals_are_dropped_by_a_proposal_with_the_same_messages() {
    for status in [
        Status::Rejected,
        Status::Closed,
        Status::ExecutionFailed,
        Status::Vetoed,
    ] {
        let ((mut deps, mut env, host), _) = adopted(&[
            Status::Open,
            Status::VetoTimelock {
                expiration: cw_utils::Expiration::Never {},
            },
        ]);
        assert_eq!(gated(&deps), vec![2, 3]);
        env.block.height += 1;
        set_status(&host, 2, status);
        // An execution reads only the entry for its own messages.
        assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
        assert_eq!(gated(&deps), vec![2, 3]);
        assert!(hook(deps.as_mut(), &env, &msgs_of(&host, 2)).is_ok());
        assert_eq!(gated(&deps), vec![3]);
    }
}

#[test]
fn proposal_without_messages_is_reported_but_never_filed() {
    // v2.4.0 marks a proposal with no messages executed without calling the core.
    let (mut deps, env, host, payload) = fixture(4);
    let empty = push(&host, env.block.height - 1, Status::Open);
    set_msgs(&host, empty, &[]);
    let response = migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(attribute(&response, "gated_proposals"), Some("2"));
    assert!(gated(&deps).is_empty());
    let mut env = env;
    env.block.height += 1;
    set_status(&host, empty, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
}

#[test]
fn executed_entry_with_other_messages_is_dropped_instead_of_refused() {
    // A key collision, or a proposal settled outside this core, must not
    // refuse a different proposal for good.
    let ((mut deps, mut env, host), _) = adopted(&[Status::Open]);
    env.block.height += 1;
    let filed = msgs_of(&host, 2);
    set_msgs(&host, 2, &fixture_msg("other"));
    set_status(&host, 2, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &filed).is_ok());
    assert!(gated(&deps).is_empty());
}

#[test]
fn boundary_height_proposals_do_not_block_later_proposals() {
    let ((mut deps, mut env, host), _) = adopted(&[]);
    let adoption_height = env.block.height;
    let open = push(&host, adoption_height, Status::Open);
    let empty = push(&host, adoption_height, Status::Executed);
    set_msgs(&host, empty, &[]);
    let dead = push(&host, adoption_height, Status::Rejected);
    env.block.height += 1;
    let later = fixture_msg("later");
    let running = push(&host, env.block.height, Status::Executed);
    set_msgs(&host, running, &later);

    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    assert_eq!(gated(&deps), vec![open]);
    assert_eq!(cursor(&deps), dead);

    let stale = msgs_of(&host, open);
    set_status(&host, open, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &stale)
        .unwrap_err()
        .to_string()
        .contains(&format!("proposal {open} predates")));
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    set_status(&host, open, Status::ExecutionFailed);
    assert!(hook(deps.as_mut(), &env, &stale).is_ok());
    assert!(gated(&deps).is_empty());
}

#[test]
fn boundary_height_proposal_is_refused_while_it_executes() {
    let ((mut deps, mut env, host), _) = adopted(&[]);
    let stale = fixture_msg("stale");
    let id = push(&host, env.block.height, Status::Executed);
    set_msgs(&host, id, &stale);
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &stale).is_err());
    // Nothing is recorded by a refused execution.
    assert_eq!(cursor(&deps), 1);
}

#[test]
fn boundary_height_scan_pages_through_every_proposal() {
    let ((mut deps, mut env, host), _) = adopted(&[]);
    for _ in 0..30 {
        push(&host, env.block.height, Status::Open);
    }
    env.block.height += 1;
    push(&host, env.block.height, Status::Open);
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
    assert_eq!(gated(&deps), (2..=31).collect::<Vec<_>>());
    assert_eq!(cursor(&deps), 31);
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
    assert_eq!(cursor(&deps), 31);
}

#[test]
fn unresolved_reviewed_proposal_still_fails() {
    let (mut deps, env, host, mut payload) = fixture(4);
    push(&host, env.block.height - 1, Status::Open);
    let MigrateMsg::AdoptPhoenix4856 {
        expected_proposal_count,
        ..
    } = &mut payload;
    *expected_proposal_count = 2;
    assert!(migrate(deps.as_mut(), env, payload)
        .unwrap_err()
        .to_string()
        .contains("resolve legacy proposal 2"));
    assert!(BOUNDARY.may_load(&deps.storage).unwrap().is_none());
    assert!(gated(&deps).is_empty());
}

#[test]
fn any_number_of_unresolved_proposals_can_be_gated() {
    let (mut deps, env, host, payload) = fixture(4);
    let open = 5 * MAX_GATED_PER_PAYLOAD;
    for _ in 0..open {
        push(&host, env.block.height - 1, Status::Open);
    }
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(gated(&deps).len(), open);

    // More created at the boundary height are filed by the next execution.
    let mut env = env;
    for _ in 0..open {
        push(&host, env.block.height, Status::Open);
    }
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
    assert_eq!(gated(&deps).len(), 2 * open);
    assert_eq!(cursor(&deps), 2 * open as u64 + 1);

    // Each one is still refused when it executes.
    let last = 2 * open as u64 + 1;
    set_status(&host, last, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &msgs_of(&host, last)).is_err());
}

#[test]
fn only_executions_sharing_messages_with_too_many_blocked_proposals_fail() {
    let (mut deps, env, host, payload) = fixture(4);
    let shared = fixture_msg("shared");
    for _ in 0..=MAX_GATED_PER_PAYLOAD {
        let id = push(&host, env.block.height - 1, Status::Open);
        set_msgs(&host, id, &shared);
    }
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(gated(&deps).len(), MAX_GATED_PER_PAYLOAD + 1);
    let mut env = env;
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &shared)
        .unwrap_err()
        .to_string()
        .contains("too many blocked proposals carry these messages"));
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
}

#[cfg(not(feature = "phoenix-rotation"))]
#[test]
fn default_build_requests_no_reply() {
    use cosmwasm_std::{Reply, SubMsgResponse, SubMsgResult};
    let ((mut deps, mut env, _), _) = adopted(&[]);
    env.block.height += 1;
    let msgs = fixture_msg("later");
    let response = hook(deps.as_mut(), &env, &msgs).unwrap();
    assert_eq!(response, Response::new().add_messages(msgs));
    let reply = Reply {
        id: 4856,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None,
        }),
    };
    assert!(matches!(
        contract::reply(deps.as_mut(), env, reply),
        Err(ContractError::UnknownReplyID {})
    ));
}

// The group and proposal module as they stand once the adoption messages ran.
#[cfg(feature = "phoenix-rotation")]
fn settled(after_review: &[Status]) -> Fixture {
    let ((deps, env, host), _) = adopted(after_review);
    {
        let mut h = host.borrow_mut();
        h.core_admin = GOV.to_owned();
        h.members.remove(0);
        h.config.threshold = Threshold::AbsoluteCount {
            threshold: YES_WEIGHT,
        };
    }
    (deps, env, host)
}

#[cfg(feature = "phoenix-rotation")]
fn swap_member() -> Vec<MemberChange> {
    vec![
        MemberChange::Remove {
            address: "fixture-member-1".to_owned(),
        },
        MemberChange::Add {
            address: "fixture-next".to_owned(),
            weight: 25,
        },
    ]
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn replacement_with_an_open_proposal_succeeds_and_gates_it() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let open = push(&host, env.block.height, Status::Open);
    env.block.height += 1;
    let response = sudo(
        deps.as_mut(),
        env.clone(),
        SudoMsg::ReplaceBoardMember {
            outgoing: "fixture-member-1".to_owned(),
            incoming: "fixture-next".to_owned(),
            expected_weight: 25,
        },
    )
    .unwrap();
    assert_eq!(response.messages.len(), 1);
    assert_eq!(attribute(&response, "gated_proposals"), Some("2"));
    assert_eq!(gated(&deps), vec![open]);
    let boundary = BOUNDARY.load(&deps.storage).unwrap();
    assert_eq!(
        (boundary.height, boundary.proposal_count),
        (env.block.height, open)
    );

    // It can no longer execute, and proposals created afterwards can.
    let stale = msgs_of(&host, open);
    set_status(&host, open, Status::Executed);
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &stale).is_err());
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn membership_update_gates_only_proposals_after_the_reviewed_count() {
    let (mut deps, mut env, host) = settled(&[Status::Open]);
    assert_eq!(gated(&deps), vec![2]);
    env.block.height += 1;
    let reviewed = push(&host, env.block.height, Status::Passed);
    let later = push(&host, env.block.height, Status::Open);
    env.block.height += 1;
    let update = |expected_proposal_count| SudoMsg::UpdateMembership {
        expected_members: host.borrow().members.clone(),
        expected_proposal_count,
        changes: swap_member(),
    };
    assert!(sudo(deps.as_mut(), env.clone(), update(later + 1))
        .unwrap_err()
        .to_string()
        .contains("proposal count is below the reviewed count"));
    assert!(sudo(deps.as_mut(), env.clone(), update(reviewed))
        .unwrap_err()
        .to_string()
        .contains(&format!("resolve legacy proposal {reviewed}")));
    assert_eq!(gated(&deps), vec![2]);

    // Earlier entries stay filed until an execution with their messages reads them.
    set_status(&host, 2, Status::Rejected);
    set_status(&host, reviewed, Status::Executed);
    let response = sudo(deps.as_mut(), env.clone(), update(reviewed)).unwrap();
    assert_eq!(attribute(&response, "gated_proposals"), Some("4"));
    assert_eq!(gated(&deps), vec![2, later]);
    assert_eq!(cursor(&deps), later);
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn execution_requests_a_reply_that_enforces_the_pinned_configuration() {
    use cosmwasm_std::ReplyOn;
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let msgs = [fixture_msg("first"), fixture_msg("last")].concat();
    let response = hook(deps.as_mut(), &env, &msgs).unwrap();
    let [first, last] = response.messages.as_slice() else {
        panic!("expected both messages")
    };
    assert_eq!((first.id, &first.reply_on), (0, &ReplyOn::Never));
    assert_eq!(
        (last.id, &last.reply_on),
        (GUARD_REPLY_ID, &ReplyOn::Success)
    );
    assert_eq!((&first.msg, &last.msg), (&msgs[0], &msgs[1]));
    assert!(hook(deps.as_mut(), &env, &[]).unwrap().messages.is_empty());

    let check = |deps: DepsMut| finish_guard(deps, &env).map_err(|err| err.to_string());
    assert_eq!(check(deps.as_mut()), Ok(Response::new()));
    assert!(GUARDED_BATCH.may_load(&deps.storage).unwrap().is_none());
    hook(deps.as_mut(), &env, &msgs).unwrap();

    let pinned = host.borrow().config.threshold.clone();
    host.borrow_mut().config.threshold = Threshold::AbsoluteCount {
        threshold: Uint128::new(50),
    };
    assert!(check(deps.as_mut())
        .unwrap_err()
        .contains("proposal threshold or DAO changed"));
    host.borrow_mut().config.threshold = pinned;

    let second = Addr::unchecked("fixture-second-module");
    PROPOSAL_MODULES
        .save(
            &mut deps.storage,
            second.clone(),
            &ProposalModule {
                address: second.clone(),
                prefix: "B".to_owned(),
                status: ProposalModuleStatus::Enabled,
            },
        )
        .unwrap();
    ACTIVE_PROPOSAL_MODULE_COUNT
        .save(&mut deps.storage, &2)
        .unwrap();
    assert!(check(deps.as_mut())
        .unwrap_err()
        .contains("unexpected active proposal modules or code"));
    PROPOSAL_MODULES.remove(&mut deps.storage, second);
    ACTIVE_PROPOSAL_MODULE_COUNT
        .save(&mut deps.storage, &1)
        .unwrap();

    host.borrow_mut().module_code = 999;
    assert!(check(deps.as_mut()).is_err());
    host.borrow_mut().module_code = 2642;

    host.borrow_mut().group_admin = GOV.to_owned();
    assert!(check(deps.as_mut())
        .unwrap_err()
        .contains("core must administer group membership"));
    host.borrow_mut().group_admin = CORE.to_owned();

    NOMINATED_ADMIN
        .save(&mut deps.storage, &Addr::unchecked("fixture-nominee"))
        .unwrap();
    assert!(check(deps.as_mut())
        .unwrap_err()
        .contains("core admin or nomination changed"));
    NOMINATED_ADMIN.remove(&mut deps.storage);

    ADMIN
        .save(&mut deps.storage, &Addr::unchecked("fixture-admin"))
        .unwrap();
    assert!(check(deps.as_mut())
        .unwrap_err()
        .contains("core admin or nomination changed"));
    ADMIN
        .save(&mut deps.storage, &Addr::unchecked(CORE))
        .unwrap();

    assert_eq!(check(deps.as_mut()), Ok(Response::new()));
}

#[cfg(feature = "phoenix-rotation")]
fn finish_guard(deps: DepsMut, env: &Env) -> Result<Response, ContractError> {
    use cosmwasm_std::{Reply, SubMsgResponse, SubMsgResult};
    let reply = Reply {
        id: GUARD_REPLY_ID,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None,
        }),
    };
    contract::reply(deps, env.clone(), reply)
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn ordinary_roster_updates_gate_proposals_from_the_previous_roster() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let open = push(&host, env.block.height, Status::Open);
    env.block.height += 1;
    let update = fixture_msg("membership change");
    hook(deps.as_mut(), &env, &update).unwrap();
    host.borrow_mut().members[0].addr = "fixture-next".to_owned();
    assert!(hook(deps.as_mut(), &env, &fixture_msg("nested proposal"))
        .unwrap_err()
        .to_string()
        .contains("nested proposal execution blocked"));
    let response = finish_guard(deps.as_mut(), &env).unwrap();
    assert_eq!(attribute(&response, "gated_proposals"), Some("2"));
    assert_eq!(gated(&deps), vec![open]);
    assert_eq!(
        BOUNDARY.load(&deps.storage).unwrap().height,
        env.block.height
    );
    assert!(hook(deps.as_mut(), &env, &fixture_msg("same block")).is_err());
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &fixture_msg("next block")).is_ok());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn ordinary_proposals_preserve_an_installed_seat_and_its_voting_rules() {
    let (mut deps, mut env, host) = settled(&[]);
    let seat = "fixture-voting-proxy".to_owned();
    let member = Member {
        addr: seat.clone(),
        weight: SEAT_WEIGHT,
    };
    governance::SEAT
        .save(
            &mut deps.storage,
            &GovernanceSeat {
                address: seat,
                code_id: 77,
            },
        )
        .unwrap();
    host.borrow_mut().members.push(member.clone());
    env.block.height += 1;
    hook(deps.as_mut(), &env, &fixture_msg("ordinary")).unwrap();
    let original_period = host.borrow().config.max_voting_period;
    host.borrow_mut().members.pop();
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("changing governance seat membership requires governance"));
    host.borrow_mut().members.push(member);
    host.borrow_mut().config.allow_revoting = true;
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("voting settings changed"));
    host.borrow_mut().config.allow_revoting = false;
    host.borrow_mut().config.max_voting_period = Duration::Height(1);
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("voting settings changed"));
    host.borrow_mut().config.max_voting_period = original_period;

    push(&host, env.block.height, Status::Open);
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("saved different voting settings"));
    host.borrow_mut()
        .proposals
        .last_mut()
        .unwrap()
        .proposal
        .expiration = original_period.after(&env.block);
    host.borrow_mut()
        .proposals
        .last_mut()
        .unwrap()
        .proposal
        .allow_revoting = true;
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("saved different voting settings"));
    host.borrow_mut()
        .proposals
        .last_mut()
        .unwrap()
        .proposal
        .allow_revoting = false;
    assert_eq!(finish_guard(deps.as_mut(), &env).unwrap(), Response::new());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn guard_checks_new_snapshots_against_protected_threshold_and_veto() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let veto = Some(VetoConfig {
        timelock_duration: Duration::Time(60),
        vetoer: GOV.to_owned(),
        early_execute: false,
        veto_before_passed: true,
    });
    host.borrow_mut().config.veto = veto.clone();
    hook(deps.as_mut(), &env, &fixture_msg("ordinary")).unwrap();
    let nested = hook(deps.as_mut(), &env, &fixture_msg("nested")).unwrap();
    assert_eq!(nested.messages[0].reply_on, cosmwasm_std::ReplyOn::Never);

    for _ in 0..PROPOSALS_PER_QUERY {
        push(&host, env.block.height, Status::Open);
        host.borrow_mut()
            .proposals
            .last_mut()
            .unwrap()
            .proposal
            .veto = veto.clone();
    }
    let id = push(&host, env.block.height, Status::Open);
    let expected = format!("proposal {id} saved different threshold or veto rules");
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains(&expected));
    let mut h = host.borrow_mut();
    let proposal = &mut h.proposals.last_mut().unwrap().proposal;
    proposal.veto = veto.clone();
    proposal.threshold = Threshold::AbsoluteCount {
        threshold: Uint128::new(50),
    };
    drop(h);
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains(&expected));
    let mut h = host.borrow_mut();
    h.proposals.last_mut().unwrap().proposal.threshold = Threshold::AbsoluteCount {
        threshold: YES_WEIGHT,
    };
    h.config.veto = None;
    drop(h);
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("proposal veto configuration changed"));
    host.borrow_mut().config.veto = veto;
    assert_eq!(finish_guard(deps.as_mut(), &env).unwrap(), Response::new());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn guard_validates_roster_and_governance_can_repair_oversized_group() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    hook(deps.as_mut(), &env, &fixture_msg("ordinary")).unwrap();
    host.borrow_mut().members[0].weight = 50;
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("require three directors"));
    let mut h = host.borrow_mut();
    h.members[0].weight = 25;
    let mut changes = vec![];
    for i in 0..4 {
        let address = format!("fixture-extra-{i}");
        h.members.push(Member {
            addr: address.clone(),
            weight: 0,
        });
        changes.push(MemberChange::Remove { address });
    }
    let expected_members = h.members.clone();
    drop(h);
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("too many group members"));
    // A rejected hook rolls all of its state back before a later sudo call.
    GUARDED_BATCH.remove(&mut deps.storage);
    let response = sudo(
        deps.as_mut(),
        env,
        SudoMsg::UpdateMembership {
            expected_members,
            expected_proposal_count: 1,
            changes,
        },
    )
    .unwrap();
    let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[0].msg else {
        panic!("expected a group update");
    };
    assert!(
        matches!(from_json(msg).unwrap(), GroupExecute::UpdateMembers { add, remove }
        if add.is_empty() && remove.len() == 4)
    );
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn checkpoint_handles_large_history_without_changing_execution_eligibility() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let open = push(&host, env.block.height, Status::Passed);
    let second = push(&host, env.block.height, Status::Open);
    for _ in 0..MAX_PROPOSALS_TO_CHECK {
        push(&host, env.block.height, Status::Closed);
    }
    let count = host.borrow().proposals.len();
    let update = |expected_proposal_count| SudoMsg::UpdateMembership {
        expected_members: host.borrow().members.clone(),
        expected_proposal_count,
        changes: swap_member(),
    };
    assert!(sudo(deps.as_mut(), env.clone(), update(1))
        .unwrap_err()
        .to_string()
        .contains("proposal count exceeds the scan window"));
    let checkpoint =
        |deps: DepsMut| sudo(deps, env.clone(), SudoMsg::CheckpointProposalHistory {}).unwrap();
    let first = checkpoint(deps.as_mut());
    assert_eq!(attribute(&first, "checked_through"), Some("49"));
    assert_eq!(
        attribute(&first, "proposal_count"),
        Some(count.to_string().as_str())
    );
    for _ in 0..(count as u64 / PROPOSALS_PER_CHECKPOINT) {
        checkpoint(deps.as_mut());
    }
    assert_eq!(
        PENDING_HISTORY.load(&deps.storage).unwrap().through,
        count as u64
    );
    assert_eq!(cursor(&deps), 1);
    assert!(gated(&deps).is_empty());

    // A staged proposal can execute until a boundary makes it stale.
    let messages = fixture_msg("still eligible");
    set_msgs(&host, open, &messages);
    set_status(&host, open, Status::Executed);
    hook(deps.as_mut(), &env, &messages).unwrap();
    finish_guard(deps.as_mut(), &env).unwrap();
    assert!(sudo(deps.as_mut(), env.clone(), update(second))
        .unwrap_err()
        .to_string()
        .contains(&format!("resolve legacy proposal {second}")));
    let response = sudo(deps.as_mut(), env, update(open)).unwrap();
    assert_eq!(attribute(&response, "gated_proposals"), Some("3"));
    assert_eq!(cursor(&deps), count as u64);
    assert_eq!(gated(&deps), vec![second]);
    assert!(PENDING_HISTORY.may_load(&deps.storage).unwrap().is_none());
}

#[test]
fn adoption_requires_the_reviewed_group_hooks() {
    let (mut deps, env, host, mut payload) = fixture(4);
    host.borrow_mut().hooks = vec!["fixture-hook".to_owned()];
    assert!(migrate(deps.as_mut(), env.clone(), payload.clone())
        .unwrap_err()
        .to_string()
        .contains("group hooks differ from the reviewed list"));
    let MigrateMsg::AdoptPhoenix4856 {
        expected_group_hooks,
        ..
    } = &mut payload;
    *expected_group_hooks = vec!["fixture-hook".to_owned()];
    migrate(deps.as_mut(), env, payload.clone()).unwrap();

    // A message written before the field existed reviews an empty list.
    let json = String::from_utf8(to_json_vec(&payload).unwrap()).unwrap();
    let field = r#""expected_group_hooks":["fixture-hook"],"#;
    assert!(json.contains(field));
    let MigrateMsg::AdoptPhoenix4856 {
        expected_group_hooks,
        ..
    } = from_json(json.replace(field, "")).unwrap();
    assert!(expected_group_hooks.is_empty());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn ordinary_proposal_cannot_change_group_hooks() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    hook(deps.as_mut(), &env, &fixture_msg("add hook")).unwrap();
    host.borrow_mut().hooks.push("fixture-hook".to_owned());
    assert!(finish_guard(deps.as_mut(), &env)
        .unwrap_err()
        .to_string()
        .contains("changing group hooks requires governance"));
    host.borrow_mut().hooks.clear();
    assert_eq!(finish_guard(deps.as_mut(), &env).unwrap(), Response::new());
}

// The reply wasmd delivers after the core instantiates a proposal module
// that returns callback messages.
#[cfg(feature = "phoenix-rotation")]
fn module_callback(deps: DepsMut, env: &Env, msgs: Vec<CosmosMsg>) -> Result<Response, String> {
    use cosmwasm_std::{Reply, SubMsgResponse, SubMsgResult};
    fn field(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        let mut len = bytes.len();
        while len >= 0x80 {
            out.push(len as u8 | 0x80);
            len >>= 7;
        }
        out.push(len as u8);
        out.extend_from_slice(bytes);
    }
    let callback =
        to_json_binary(&dao_interface::state::ModuleInstantiateCallback { msgs }).unwrap();
    let mut data = Vec::new();
    field(0x0a, b"fixture-new-module", &mut data);
    field(0x12, callback.as_slice(), &mut data);
    let reply = Reply {
        id: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: Some(data.into()),
        }),
    };
    contract::reply(deps, env.clone(), reply).map_err(|err| err.to_string())
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn new_module_callbacks_get_the_same_check_as_proposal_messages() {
    let (mut deps, mut env, _) = settled(&[]);
    env.block.height += 1;
    crate::state::TOTAL_PROPOSAL_MODULE_COUNT
        .save(&mut deps.storage, &1)
        .unwrap();
    let takeover: CosmosMsg = WasmMsg::UpdateAdmin {
        contract_addr: GROUP.to_owned(),
        admin: "fixture-attacker".to_owned(),
    }
    .into();

    // Outside a proposal the core is acting for governance or at instantiation.
    assert!(module_callback(deps.as_mut(), &env, vec![takeover.clone()]).is_ok());

    hook(deps.as_mut(), &env, &fixture_msg("add module")).unwrap();
    assert!(module_callback(deps.as_mut(), &env, vec![takeover])
        .unwrap_err()
        .contains("companion contract administration requires governance"));
    let harmless = fixture_msg("callback");
    let response = module_callback(deps.as_mut(), &env, harmless.clone()).unwrap();
    assert_eq!(response.messages[0].msg, harmless[0]);
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn governance_send_that_changes_the_roster_sets_a_boundary() {
    use cosmwasm_std::{Reply, ReplyOn, SubMsgResponse, SubMsgResult};
    let (mut deps, mut env, host) = settled(&[]);
    sudo(
        deps.as_mut(),
        env.clone(),
        SudoMsg::SetSendAsDao { enabled: true },
    )
    .unwrap();
    env.block.time = env.block.time.plus_seconds(48 * 60 * 60);
    env.block.height += 1;
    let open = push(&host, env.block.height, Status::Open);
    env.block.height += 1;
    let adoption = BOUNDARY.load(&deps.storage).unwrap();
    let send = SudoMsg::SendAsDao {
        messages: [fixture_msg("first"), fixture_msg("last")].concat(),
    };
    let finish = |deps: DepsMut| {
        let reply = Reply {
            id: SEND_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
            }),
        };
        contract::reply(deps, env.clone(), reply).unwrap()
    };

    // Messages that leave the roster alone leave proposals executable.
    let response = sudo(deps.as_mut(), env.clone(), send.clone()).unwrap();
    let [first, last] = response.messages.as_slice() else {
        panic!("expected both messages")
    };
    assert_eq!((first.id, &first.reply_on), (0, &ReplyOn::Never));
    assert_eq!(
        (last.id, &last.reply_on),
        (SEND_REPLY_ID, &ReplyOn::Success)
    );
    assert_eq!(finish(deps.as_mut()), Response::new());
    assert_eq!(BOUNDARY.load(&deps.storage).unwrap(), adoption);
    assert!(gated(&deps).is_empty());

    // A roster change blocks the proposals that snapshotted the old roster.
    sudo(deps.as_mut(), env.clone(), send).unwrap();
    host.borrow_mut().members[0].addr = "fixture-next".to_owned();
    let response = finish(deps.as_mut());
    assert_eq!(
        attribute(&response, "membership_boundary"),
        Some(env.block.height.to_string().as_str())
    );
    assert_eq!(attribute(&response, "gated_proposals"), Some("2"));
    assert_eq!(gated(&deps), vec![open]);
    let boundary = BOUNDARY.load(&deps.storage).unwrap();
    assert_eq!(
        (boundary.height, boundary.proposal_count),
        (env.block.height, open)
    );
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn two_membership_actions_succeed_in_one_block() {
    let (mut deps, mut env, host) = settled(&[]);
    env.block.height += 1;
    let replace = |outgoing: &str, incoming: &str| SudoMsg::ReplaceBoardMember {
        outgoing: outgoing.to_owned(),
        incoming: incoming.to_owned(),
        expected_weight: SEAT_WEIGHT,
    };
    sudo(
        deps.as_mut(),
        env.clone(),
        replace("fixture-member-1", "fixture-next-1"),
    )
    .unwrap();
    {
        // The first update has applied. The block-start snapshot no longer
        // matches the roster the second action reads.
        let mut h = host.borrow_mut();
        h.members[0].addr = "fixture-next-1".to_owned();
        h.block_start_weight = Some(0);
    }
    sudo(
        deps.as_mut(),
        env.clone(),
        replace("fixture-member-2", "fixture-next-2"),
    )
    .unwrap();
    assert_eq!(
        BOUNDARY.load(&deps.storage).unwrap().height,
        env.block.height
    );
}
