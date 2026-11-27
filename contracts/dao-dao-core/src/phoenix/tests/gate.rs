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
        proposal: ProposalSnapshot {
            start_height,
            status,
        },
    });
    id
}

fn set_status(host: &Rc<RefCell<Host>>, id: u64, status: Status) {
    host.borrow_mut().proposals[id as usize - 1].proposal.status = status;
}

fn set_msgs(host: &Rc<RefCell<Host>>, id: u64, msgs: &[CosmosMsg]) {
    host.borrow_mut().messages.insert(id, msgs.to_vec());
}

fn gated(deps: &OwnedDeps<MockStorage, MockApi, MockQuerier>) -> Vec<u64> {
    GATED.load(&deps.storage).unwrap()
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
    assert!(GATED.may_load(&deps.storage).unwrap().is_none());
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
    let stale = fixture_msg("stale");
    set_msgs(&host, 2, &stale);
    set_status(&host, 2, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &stale)
        .unwrap_err()
        .to_string()
        .contains("proposal 2 predates the current membership"));
    assert_eq!(gated(&deps), vec![2]);

    // The module records the failure, and the entry is pruned.
    set_status(&host, 2, Status::ExecutionFailed);
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    assert!(gated(&deps).is_empty());
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
}

#[test]
fn adoption_without_unresolved_proposals_reports_none() {
    let ((deps, _, _), response) = adopted(&[Status::Executed, Status::Rejected]);
    assert_eq!(attribute(&response, "gated_proposals"), None);
    assert!(gated(&deps).is_empty());
    assert_eq!(cursor(&deps), 3);
}

#[test]
fn settled_gated_proposals_are_pruned() {
    for status in [
        Status::Rejected,
        Status::Closed,
        Status::ExecutionFailed,
        Status::Vetoed,
        // v2.4.0 marks a proposal with no messages executed without calling the core.
        Status::Executed,
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
        assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
        assert_eq!(gated(&deps), vec![3]);
    }
}

#[test]
fn executed_gated_proposal_with_other_messages_does_not_freeze_execution() {
    let ((mut deps, mut env, host), _) = adopted(&[Status::Open]);
    env.block.height += 1;
    set_msgs(&host, 2, &fixture_msg("stale"));
    set_status(&host, 2, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
    assert!(gated(&deps).is_empty());
}

#[test]
fn boundary_height_proposals_do_not_block_later_proposals() {
    let ((mut deps, mut env, host), _) = adopted(&[]);
    let adoption_height = env.block.height;
    let open = push(&host, adoption_height, Status::Open);
    let empty = push(&host, adoption_height, Status::Executed);
    let dead = push(&host, adoption_height, Status::Rejected);
    env.block.height += 1;
    let later = fixture_msg("later");
    let running = push(&host, env.block.height, Status::Executed);
    set_msgs(&host, running, &later);

    assert!(hook(deps.as_mut(), &env, &later).is_ok());
    assert_eq!(gated(&deps), vec![open]);
    assert_eq!(cursor(&deps), dead);
    let _ = empty;

    let stale = fixture_msg("stale");
    set_msgs(&host, open, &stale);
    set_status(&host, open, Status::Executed);
    assert!(hook(deps.as_mut(), &env, &stale)
        .unwrap_err()
        .to_string()
        .contains(&format!("proposal {open} predates")));
    assert!(hook(deps.as_mut(), &env, &later).is_ok());
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
    assert!(GATED.may_load(&deps.storage).unwrap().is_none());
}

#[test]
fn gated_proposal_limit_fails_closed() {
    let (mut deps, env, host, payload) = fixture(4);
    for _ in 0..MAX_GATED {
        push(&host, env.block.height - 1, Status::Open);
    }
    let over = push(&host, env.block.height - 1, Status::Open);
    assert!(migrate(deps.as_mut(), env.clone(), payload.clone())
        .unwrap_err()
        .to_string()
        .contains("too many unresolved proposals"));
    set_status(&host, over, Status::Rejected);
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(gated(&deps).len(), MAX_GATED);

    // One more created at the boundary height blocks execution until any expires.
    let mut env = env;
    let tail = push(&host, env.block.height, Status::Open);
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later"))
        .unwrap_err()
        .to_string()
        .contains("too many unresolved proposals"));
    set_status(&host, 2, Status::Rejected);
    assert!(hook(deps.as_mut(), &env, &fixture_msg("later")).is_ok());
    assert_eq!(gated(&deps).len(), MAX_GATED);
    assert_eq!(gated(&deps).last(), Some(&tail));
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
    let stale = fixture_msg("stale");
    set_msgs(&host, open, &stale);
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

    // The earlier gated proposal expired, so it is dropped rather than carried.
    set_status(&host, 2, Status::Rejected);
    set_status(&host, reviewed, Status::Executed);
    let response = sudo(deps.as_mut(), env.clone(), update(reviewed)).unwrap();
    assert_eq!(attribute(&response, "gated_proposals"), Some("4"));
    assert_eq!(gated(&deps), vec![later]);
    assert_eq!(cursor(&deps), later);
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn execution_requests_a_reply_that_enforces_the_pinned_configuration() {
    use cosmwasm_std::{Reply, ReplyOn, SubMsgResponse, SubMsgResult};
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

    let check = |deps: DepsMut| {
        let reply = Reply {
            id: GUARD_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
            }),
        };
        contract::reply(deps, env.clone(), reply).map_err(|err| err.to_string())
    };
    assert_eq!(check(deps.as_mut()), Ok(Response::new()));

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
