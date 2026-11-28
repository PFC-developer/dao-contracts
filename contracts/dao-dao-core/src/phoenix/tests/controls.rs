use super::*;

const SEAT_ADDR: &str = "fixture-seat";
#[cfg(feature = "phoenix-rotation")]
const OTHER_SEAT: &str = "fixture-seat-next";
const SEAT_CODE: u64 = 77;

#[cfg(feature = "phoenix-rotation")]
type MockDeps = OwnedDeps<MockStorage, MockApi, MockQuerier>;

#[cw_serde]
struct SeatConfigFixture {
    dao: String,
    group: String,
    proposal_module: String,
    seat_weight: u64,
    response_blocks: u64,
}

/// Serve contract info and the bound config for the fixture seat contracts.
fn install_seats(host: &Rc<RefCell<Host>>) -> Rc<RefCell<u64>> {
    let code = Rc::new(RefCell::new(SEAT_CODE));
    let live = code.clone();
    host.borrow_mut().extra = Some(Box::new(move |query| match query {
        WasmQuery::ContractInfo { contract_addr } if contract_addr.starts_with(SEAT_ADDR) => {
            let mut info = ContractInfoResponse::default();
            info.code_id = *live.borrow();
            info.admin = Some(GOV.to_owned());
            Some(to_json_binary(&info).unwrap())
        }
        WasmQuery::Smart { contract_addr, .. } if contract_addr.starts_with(SEAT_ADDR) => Some(
            to_json_binary(&SeatConfigFixture {
                dao: CORE.to_owned(),
                group: GROUP.to_owned(),
                proposal_module: PROPOSAL.to_owned(),
                seat_weight: SEAT_WEIGHT,
                response_blocks: 6,
            })
            .unwrap(),
        ),
        _ => None,
    }));
    code
}

fn seat(code_id: u64) -> GovernanceSeat {
    GovernanceSeat {
        address: SEAT_ADDR.to_owned(),
        code_id,
    }
}

fn options(seat: Option<GovernanceSeat>) -> GovernanceOptions {
    GovernanceOptions {
        migration_admin: MigrationAdmin::GovernanceOnly,
        allow_send_as_dao: false,
        seat,
        seat_veto_only: false,
        membership_changes: vec![],
        veto: None,
    }
}

fn with_options(payload: &mut MigrateMsg, options: GovernanceOptions) {
    let MigrateMsg::AdoptPhoenix4856 { governance, .. } = payload;
    *governance = Some(options);
}

fn group_update(response: &Response) -> GroupExecute {
    let CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr, msg, ..
    }) = &response.messages[0].msg
    else {
        panic!("expected group update")
    };
    assert_eq!(contract_addr, GROUP);
    from_json(msg).unwrap()
}

/// Bring the mocked chain to the state the adoption messages produce.
#[cfg(feature = "phoenix-rotation")]
fn settle(host: &Rc<RefCell<Host>>, seated: bool) {
    let mut h = host.borrow_mut();
    h.core_admin = GOV.to_owned();
    h.members.remove(0);
    if seated {
        h.members.push(Member {
            addr: SEAT_ADDR.to_owned(),
            weight: SEAT_WEIGHT,
        });
    }
    h.config.threshold = Threshold::AbsoluteCount {
        threshold: YES_WEIGHT,
    };
}

/// Adopt with `size` reviewed members and a seated governance voter.
#[cfg(feature = "phoenix-rotation")]
fn adopted_with_seat(size: usize) -> (MockDeps, Env, Rc<RefCell<Host>>, Rc<RefCell<u64>>) {
    let (mut deps, env, host, mut payload) = fixture(size);
    let code = install_seats(&host);
    with_options(&mut payload, options(Some(seat(SEAT_CODE))));
    let response = migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(
        group_update(&response),
        GroupExecute::UpdateMembers {
            add: vec![Member {
                addr: SEAT_ADDR.to_owned(),
                weight: SEAT_WEIGHT
            }],
            remove: vec!["fixture-member-0".to_owned()],
        }
    );
    settle(&host, true);
    (deps, env, host, code)
}

#[cfg(feature = "phoenix-rotation")]
fn replace(outgoing: &str, incoming: &str) -> SudoMsg {
    SudoMsg::ReplaceBoardMember {
        outgoing: outgoing.to_owned(),
        incoming: incoming.to_owned(),
        expected_weight: SEAT_WEIGHT,
    }
}

#[cfg(feature = "phoenix-rotation")]
fn update(host: &Rc<RefCell<Host>>, changes: Vec<MemberChange>) -> SudoMsg {
    SudoMsg::UpdateMembership {
        expected_members: host.borrow().members.clone(),
        expected_proposal_count: 1,
        changes,
    }
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn seat_is_reapproved_after_its_code_changes() {
    let (mut deps, env, _, code) = adopted_with_seat(5);
    let rotation = replace("fixture-member-1", "fixture-next");
    *code.borrow_mut() = SEAT_CODE + 1;
    assert!(sudo(deps.as_mut(), env.clone(), rotation.clone())
        .unwrap_err()
        .to_string()
        .contains("unexpected governance seat code or admin"));
    // Only the live code can be approved.
    assert!(sudo(
        deps.as_mut(),
        env.clone(),
        SudoMsg::SetGovernanceSeat {
            seat: Some(seat(SEAT_CODE))
        }
    )
    .is_err());
    let response = sudo(
        deps.as_mut(),
        env.clone(),
        SudoMsg::SetGovernanceSeat {
            seat: Some(seat(SEAT_CODE + 1)),
        },
    )
    .unwrap();
    assert!(response.messages.is_empty());
    assert_eq!(
        governance::SEAT.load(&deps.storage).unwrap(),
        seat(SEAT_CODE + 1)
    );
    assert!(sudo(deps.as_mut(), env, rotation).is_ok());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn seat_address_changes_and_clears_only_while_unseated() {
    let (mut deps, env, host, _) = adopted_with_seat(5);
    let other = GovernanceSeat {
        address: OTHER_SEAT.to_owned(),
        code_id: SEAT_CODE,
    };
    let set = |seat| SudoMsg::SetGovernanceSeat { seat };
    // The stored seat still holds a ballot.
    assert!(sudo(deps.as_mut(), env.clone(), set(Some(other.clone()))).is_err());
    assert!(sudo(deps.as_mut(), env.clone(), set(None))
        .unwrap_err()
        .to_string()
        .contains("remove the governance seat from the group"));
    assert_eq!(
        governance::SEAT.load(&deps.storage).unwrap(),
        seat(SEAT_CODE)
    );
    // The stored seat has left, but the replacement address is already a member.
    host.borrow_mut().members.pop();
    host.borrow_mut().members.push(Member {
        addr: OTHER_SEAT.to_owned(),
        weight: SEAT_WEIGHT,
    });
    assert!(sudo(deps.as_mut(), env.clone(), set(Some(other.clone()))).is_err());
    host.borrow_mut().members.pop();
    sudo(deps.as_mut(), env.clone(), set(Some(other.clone()))).unwrap();
    assert_eq!(governance::SEAT.load(&deps.storage).unwrap(), other);
    let response = sudo(deps.as_mut(), env.clone(), set(None)).unwrap();
    assert!(response
        .attributes
        .iter()
        .any(|a| a.key == "seat" && a.value == OTHER_SEAT));
    assert!(governance::SEAT.may_load(&deps.storage).unwrap().is_none());
    assert!(sudo(deps.as_mut(), env.clone(), set(None)).is_err());
    // With no stored seat, a new one can be approved while it holds no ballot.
    sudo(deps.as_mut(), env, set(Some(seat(SEAT_CODE)))).unwrap();
    assert_eq!(
        governance::SEAT.load(&deps.storage).unwrap(),
        seat(SEAT_CODE)
    );
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn unapproved_seat_code_blocks_only_actions_that_keep_it_seated() {
    let (mut deps, env, host, code) = adopted_with_seat(5);
    *code.borrow_mut() = SEAT_CODE + 1;
    let keep = update(
        &host,
        vec![MemberChange::Remove {
            address: "fixture-member-1".to_owned(),
        }],
    );
    assert!(sudo(deps.as_mut(), env.clone(), keep)
        .unwrap_err()
        .to_string()
        .contains("unexpected governance seat code or admin"));
    let remove = update(
        &host,
        vec![MemberChange::Remove {
            address: SEAT_ADDR.to_owned(),
        }],
    );
    let response = sudo(deps.as_mut(), env.clone(), remove).unwrap();
    assert_eq!(
        group_update(&response),
        GroupExecute::UpdateMembers {
            add: vec![],
            remove: vec![SEAT_ADDR.to_owned()],
        }
    );
    // The same holds for the single-seat replacement action.
    let (mut deps, env, _, code) = adopted_with_seat(5);
    *code.borrow_mut() = SEAT_CODE + 1;
    assert!(sudo(
        deps.as_mut(),
        env.clone(),
        replace("fixture-member-1", "fixture-next")
    )
    .is_err());
    assert!(sudo(deps.as_mut(), env, replace(SEAT_ADDR, "fixture-next")).is_ok());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn membership_actions_restore_threshold_and_proposal_modules() {
    let other = Addr::unchecked("fixture-second-module");
    for primary_disabled in [false, true] {
        let (mut deps, env, host, payload) = fixture(4);
        migrate(deps.as_mut(), env.clone(), payload).unwrap();
        settle(&host, false);
        // The board lowers the threshold and routes execution to another module.
        let drifted = Threshold::AbsoluteCount {
            threshold: Uint128::new(50),
        };
        host.borrow_mut().config.threshold = drifted;
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
        ACTIVE_PROPOSAL_MODULE_COUNT
            .save(&mut deps.storage, &2)
            .unwrap();
        if primary_disabled {
            PROPOSAL_MODULES
                .update::<_, StdError>(&mut deps.storage, Addr::unchecked(PROPOSAL), |m| {
                    let mut m = m.unwrap();
                    m.status = ProposalModuleStatus::Disabled;
                    Ok(m)
                })
                .unwrap();
            ACTIVE_PROPOSAL_MODULE_COUNT
                .save(&mut deps.storage, &1)
                .unwrap();
        }
        let action = if primary_disabled {
            replace("fixture-member-1", "fixture-next")
        } else {
            update(
                &host,
                vec![MemberChange::Add {
                    address: "fixture-next".to_owned(),
                    weight: SEAT_WEIGHT,
                }],
            )
        };
        let response = sudo(deps.as_mut(), env, action).unwrap();
        assert_eq!(response.messages.len(), 2);
        assert!(matches!(
            group_update(&response),
            GroupExecute::UpdateMembers { .. }
        ));
        let CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) = &response.messages[1].msg
        else {
            panic!("expected config update")
        };
        assert_eq!(contract_addr, PROPOSAL);
        assert!(funds.is_empty());
        let mut expected = host.borrow().config.clone();
        expected.threshold = Threshold::AbsoluteCount {
            threshold: YES_WEIGHT,
        };
        assert_eq!(
            from_json::<ProposalExecute>(msg).unwrap(),
            ProposalExecute::UpdateConfig(expected)
        );
        let status = |deps: &MockDeps, addr: &str| {
            PROPOSAL_MODULES
                .load(&deps.storage, Addr::unchecked(addr))
                .unwrap()
                .status
        };
        assert_eq!(status(&deps, PROPOSAL), ProposalModuleStatus::Enabled);
        assert_eq!(
            status(&deps, other.as_str()),
            ProposalModuleStatus::Disabled
        );
        assert_eq!(ACTIVE_PROPOSAL_MODULE_COUNT.load(&deps.storage).unwrap(), 1);
        for (key, value) in [
            (
                "restored_proposal_modules",
                if primary_disabled { "2" } else { "1" },
            ),
            ("restored_threshold", "75"),
        ] {
            assert!(response
                .attributes
                .iter()
                .any(|a| a.key == key && a.value == value));
        }
    }
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn membership_actions_still_fail_on_drift_the_core_cannot_restore() {
    for fail in 0..4 {
        let (mut deps, env, host, payload) = fixture(4);
        migrate(deps.as_mut(), env.clone(), payload).unwrap();
        settle(&host, false);
        match fail {
            0 => host.borrow_mut().module_code = 999,
            1 => host.borrow_mut().config.dao = "fixture-other-dao".to_owned(),
            2 => host.borrow_mut().group_admin = "fixture-other-admin".to_owned(),
            _ => PROPOSAL_MODULES.remove(&mut deps.storage, Addr::unchecked(PROPOSAL)),
        }
        assert!(
            sudo(
                deps.as_mut(),
                env.clone(),
                replace("fixture-member-1", "fixture-next")
            )
            .is_err(),
            "case {fail}"
        );
        let add = update(
            &host,
            vec![MemberChange::Add {
                address: "fixture-next".to_owned(),
                weight: SEAT_WEIGHT,
            }],
        );
        assert!(sudo(deps.as_mut(), env, add).is_err(), "case {fail}");
    }
}

#[test]
fn seated_seat_allows_at_most_four_directors() {
    let (deps, _, host, _) = fixture(4);
    install_seats(&host);
    let approved = seat(SEAT_CODE);
    let roster = |directors: usize| {
        (0..directors)
            .map(|i| (format!("fixture-member-{i}"), SEAT_WEIGHT))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let seat_in = || {
        vec![MemberChange::Add {
            address: SEAT_ADDR.to_owned(),
            weight: SEAT_WEIGHT,
        }]
    };
    for (directors, ok) in [(3, true), (4, true), (5, false)] {
        let plan = governance::plan_membership(
            deps.as_ref(),
            roster(directors),
            seat_in(),
            Some(&approved),
        );
        assert_eq!(plan.is_ok(), ok, "{directors} directors");
        if let Err(error) = plan {
            assert!(error.to_string().contains("three or four directors"));
        }
    }
    // Five directors remain valid while the approved seat holds no ballot.
    let mut six = roster(6);
    assert!(governance::plan_membership(
        deps.as_ref(),
        six.clone(),
        vec![MemberChange::Remove {
            address: "fixture-member-5".to_owned()
        }],
        Some(&approved),
    )
    .is_ok());
    // A fifth director cannot join a seated seat either.
    six.remove("fixture-member-5");
    six.remove("fixture-member-4");
    six.insert(SEAT_ADDR.to_owned(), SEAT_WEIGHT);
    assert!(governance::plan_membership(
        deps.as_ref(),
        six,
        vec![MemberChange::Add {
            address: "fixture-next".to_owned(),
            weight: SEAT_WEIGHT
        }],
        Some(&approved),
    )
    .is_err());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn adoption_refuses_five_directors_with_a_seated_seat() {
    let (mut deps, env, host, mut payload) = fixture(5);
    install_seats(&host);
    let mut opts = options(Some(seat(SEAT_CODE)));
    opts.membership_changes = vec![MemberChange::Add {
        address: "fixture-next".to_owned(),
        weight: SEAT_WEIGHT,
    }];
    with_options(&mut payload, opts);
    assert!(migrate(deps.as_mut(), env, payload)
        .unwrap_err()
        .to_string()
        .contains("three or four directors"));
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn adoption_configures_voting_rules_for_a_governance_seat() {
    for period in [6, 7] {
        let (mut deps, env, host, mut payload) = fixture(4);
        install_seats(&host);
        let config = {
            let mut h = host.borrow_mut();
            h.config.allow_revoting = true;
            h.config.max_voting_period = Duration::Height(period);
            h.config.clone()
        };
        let MigrateMsg::AdoptPhoenix4856 {
            expected_proposal_config,
            ..
        } = &mut payload;
        *expected_proposal_config = config;
        with_options(&mut payload, options(Some(seat(SEAT_CODE))));
        let result = migrate(deps.as_mut(), env, payload);
        if period == 6 {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("must outlast governance response window"));
        } else {
            let response = result.unwrap();
            let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[1].msg else {
                panic!("expected configuration update");
            };
            let ProposalExecute::UpdateConfig(config) = from_json(msg).unwrap();
            assert!(!config.allow_revoting);
            assert_eq!(config.max_voting_period, Duration::Height(7));
        }
    }
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn veto_only_seat_is_recorded_without_a_ballot_and_can_be_seated_later() {
    let veto = |vetoer: &str| VetoConfig {
        timelock_duration: Duration::Time(86400),
        vetoer: vetoer.to_owned(),
        early_execute: false,
        veto_before_passed: true,
    };
    let veto_only = |veto: Option<VetoConfig>| {
        let mut opts = options(Some(seat(SEAT_CODE)));
        opts.seat_veto_only = true;
        opts.veto = veto;
        opts
    };
    for refused in [None, Some(veto(GOV))] {
        let (mut deps, env, host, mut payload) = fixture(4);
        install_seats(&host);
        with_options(&mut payload, veto_only(refused));
        assert!(migrate(deps.as_mut(), env, payload)
            .unwrap_err()
            .to_string()
            .contains("veto-only seat must be the configured vetoer"));
    }
    let (mut deps, env, host, mut payload) = fixture(4);
    install_seats(&host);
    let mut missing_seat = veto_only(Some(veto(SEAT_ADDR)));
    missing_seat.seat = None;
    with_options(&mut payload, missing_seat);
    assert!(migrate(deps.as_mut(), env.clone(), payload.clone()).is_err());

    with_options(&mut payload, veto_only(Some(veto(SEAT_ADDR))));
    let response = migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(
        group_update(&response),
        GroupExecute::UpdateMembers {
            add: vec![],
            remove: vec!["fixture-member-0".to_owned()],
        }
    );
    let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[1].msg else {
        panic!("expected config update")
    };
    let ProposalExecute::UpdateConfig(config) = from_json(msg).unwrap();
    assert_eq!(config.veto, Some(veto(SEAT_ADDR)));
    assert_eq!(
        governance::SEAT.load(&deps.storage).unwrap(),
        seat(SEAT_CODE)
    );
    assert!(response
        .attributes
        .iter()
        .any(|a| a.key == "remaining_seats" && a.value == "3"));

    // Seating it later goes through the membership action and its equal-weight rule.
    settle(&host, false);
    let seat_in = |weight| {
        update(
            &host,
            vec![MemberChange::Add {
                address: SEAT_ADDR.to_owned(),
                weight,
            }],
        )
    };
    assert!(sudo(deps.as_mut(), env.clone(), seat_in(SEAT_WEIGHT - 1)).is_err());
    let response = sudo(deps.as_mut(), env, seat_in(SEAT_WEIGHT)).unwrap();
    assert_eq!(
        group_update(&response),
        GroupExecute::UpdateMembers {
            add: vec![Member {
                addr: SEAT_ADDR.to_owned(),
                weight: SEAT_WEIGHT
            }],
            remove: vec![],
        }
    );
}

#[test]
fn governance_veto_requires_open_and_post_passage_windows() {
    let veto = |duration, veto_before_passed| VetoConfig {
        timelock_duration: duration,
        vetoer: GOV.to_owned(),
        early_execute: false,
        veto_before_passed,
    };
    for refused in [
        veto(Duration::Time(86_400), false),
        veto(Duration::Time(0), true),
        veto(Duration::Height(0), true),
    ] {
        let (mut deps, env, _, mut payload) = fixture(4);
        let mut opts = options(None);
        opts.veto = Some(refused);
        with_options(&mut payload, opts);
        assert!(migrate(deps.as_mut(), env, payload)
            .unwrap_err()
            .to_string()
            .contains("must cover open and post-passage proposals"));
    }

    for duration in [Duration::Time(1), Duration::Height(1)] {
        let (mut deps, env, host, mut payload) = fixture(4);
        if matches!(duration, Duration::Height(_)) {
            host.borrow_mut().config.max_voting_period = Duration::Height(10);
            let MigrateMsg::AdoptPhoenix4856 {
                expected_proposal_config,
                ..
            } = &mut payload;
            expected_proposal_config.max_voting_period = Duration::Height(10);
        }
        let mut opts = options(None);
        opts.veto = Some(veto(duration, true));
        with_options(&mut payload, opts);
        assert!(migrate(deps.as_mut(), env, payload).is_ok());
    }
}

#[cfg(not(feature = "phoenix-rotation"))]
#[test]
fn default_build_refuses_a_veto_only_seat() {
    let (mut deps, env, host, mut payload) = fixture(4);
    install_seats(&host);
    let mut opts = options(None);
    opts.migration_admin = MigrationAdmin::KeepCurrentAdmin;
    with_options(&mut payload, opts.clone());
    let response = migrate(deps.as_mut(), env, payload).unwrap();
    assert_eq!(
        group_update(&response),
        GroupExecute::UpdateMembers {
            add: vec![],
            remove: vec!["fixture-member-0".to_owned()],
        }
    );

    let (mut deps, env, host, mut payload) = fixture(4);
    install_seats(&host);
    opts.seat_veto_only = true;
    with_options(&mut payload, opts);
    assert!(migrate(deps.as_mut(), env, payload)
        .unwrap_err()
        .to_string()
        .contains("not enabled in this build"));
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn reapproving_a_seated_seat_reapplies_its_voting_rules() {
    let (mut deps, env, host, _) = adopted_with_seat(4);
    let reapprove = SudoMsg::SetGovernanceSeat {
        seat: Some(seat(SEAT_CODE)),
    };

    // The voting rules already fit the seat's response window.
    let response = sudo(deps.as_mut(), env.clone(), reapprove.clone()).unwrap();
    assert!(response.messages.is_empty());

    // The fixture seat reports a six-block response window.
    host.borrow_mut().config.max_voting_period = Duration::Height(6);
    assert!(sudo(deps.as_mut(), env.clone(), reapprove.clone())
        .unwrap_err()
        .to_string()
        .contains("voting period must outlast governance response window"));

    let mut h = host.borrow_mut();
    h.config.max_voting_period = Duration::Height(7);
    h.config.allow_revoting = true;
    let mut expected = h.config.clone();
    expected.allow_revoting = false;
    drop(h);
    let response = sudo(deps.as_mut(), env.clone(), reapprove.clone()).unwrap();
    let CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr, msg, ..
    }) = &response.messages[0].msg
    else {
        panic!("expected a proposal config update")
    };
    assert_eq!(contract_addr, PROPOSAL);
    assert_eq!(
        from_json::<ProposalExecute>(msg).unwrap(),
        ProposalExecute::UpdateConfig(expected)
    );

    // A seat that holds no ballot does not constrain the voting rules.
    host.borrow_mut().members.pop();
    let response = sudo(deps.as_mut(), env, reapprove).unwrap();
    assert!(response.messages.is_empty());
}
