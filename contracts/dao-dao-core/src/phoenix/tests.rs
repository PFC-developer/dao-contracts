use std::{cell::RefCell, rc::Rc};

use cosmwasm_std::{
    from_json,
    testing::{mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage},
    Binary, CodeInfoResponse, ContractInfoResponse, ContractResult, CosmosMsg, Decimal, OwnedDeps,
    SystemResult, WasmQuery,
};
use cw2::set_contract_version;
use cw4::Member;
use dao_interface::state::ProposalModule;
use dao_voting::threshold::PercentageThreshold;

use super::*;

type ExtraQueries = Box<dyn Fn(&WasmQuery) -> Option<Binary>>;

struct Host {
    /// Answers queries for contracts outside the base fixture, such as a governance seat.
    extra: Option<ExtraQueries>,
    members: Vec<Member>,
    old_code: u64,
    core_admin: String,
    group_admin: String,
    module_code: u64,
    checksum: HexBinary,
    block_start_weight: Option<u64>,
    config: ProposalConfig,
    proposals: Vec<ProposalRecord>,
    /// Messages of the proposals that carry any, by proposal ID.
    messages: std::collections::BTreeMap<u64, Vec<CosmosMsg>>,
}

// The single-proposal query as the deployed module answers it, with messages.
#[derive(serde::Serialize)]
struct FullRecord<'a> {
    id: u64,
    proposal: FullProposal<'a>,
}

#[derive(serde::Serialize)]
struct FullProposal<'a> {
    start_height: u64,
    status: &'a Status,
    msgs: &'a [CosmosMsg],
    #[cfg(feature = "phoenix-rotation")]
    threshold: &'a Threshold,
    #[cfg(feature = "phoenix-rotation")]
    veto: &'a Option<VetoConfig>,
    #[cfg(feature = "phoenix-rotation")]
    allow_revoting: bool,
    #[cfg(feature = "phoenix-rotation")]
    expiration: cw_utils::Expiration,
}

fn snapshot(start_height: u64, status: Status) -> ProposalSnapshot {
    ProposalSnapshot {
        start_height,
        status,
        #[cfg(feature = "phoenix-rotation")]
        threshold: Threshold::AbsoluteCount {
            threshold: YES_WEIGHT,
        },
        #[cfg(feature = "phoenix-rotation")]
        veto: None,
        #[cfg(feature = "phoenix-rotation")]
        allow_revoting: false,
        #[cfg(feature = "phoenix-rotation")]
        expiration: cw_utils::Expiration::Never {},
    }
}

fn fixture(
    size: usize,
) -> (
    OwnedDeps<MockStorage, MockApi, MockQuerier>,
    Env,
    Rc<RefCell<Host>>,
    MigrateMsg,
) {
    let mut deps = mock_dependencies();
    let mut env = mock_env();
    env.block.chain_id = "phoenix-1".to_owned();
    env.contract.address = Addr::unchecked(CORE);
    set_contract_version(&mut deps.storage, contract::CONTRACT_NAME, "2.4.0").unwrap();
    ADMIN
        .save(&mut deps.storage, &Addr::unchecked(CORE))
        .unwrap();
    VOTING_MODULE
        .save(&mut deps.storage, &Addr::unchecked(VOTING))
        .unwrap();
    ACTIVE_PROPOSAL_MODULE_COUNT
        .save(&mut deps.storage, &1)
        .unwrap();
    PROPOSAL_MODULES
        .save(
            &mut deps.storage,
            Addr::unchecked(PROPOSAL),
            &ProposalModule {
                address: Addr::unchecked(PROPOSAL),
                prefix: "A".to_owned(),
                status: ProposalModuleStatus::Enabled,
            },
        )
        .unwrap();
    let addresses: Vec<_> = (0..size).map(|i| format!("fixture-member-{i}")).collect();
    let config = ProposalConfig {
        threshold: Threshold::AbsolutePercentage {
            percentage: PercentageThreshold::Percent(Decimal::percent(75)),
        },
        max_voting_period: Duration::Time(604800),
        min_voting_period: None,
        only_members_execute: true,
        allow_revoting: false,
        dao: CORE.to_owned(),
        close_proposal_on_execution_failure: true,
        veto: None,
    };
    let payload = MigrateMsg::AdoptPhoenix4856 {
        expected_members: addresses.clone(),
        outgoing: addresses[0].clone(),
        expected_proposal_config: config.clone(),
        expected_creation_policy: ProposalCreationPolicy::Anyone {},
        expected_proposal_count: 1,
        governance: None,
    };
    let host = Rc::new(RefCell::new(Host {
        extra: None,
        members: addresses
            .into_iter()
            .map(|addr| Member { addr, weight: 25 })
            .collect(),
        old_code: OLD_CODE_ID,
        core_admin: CORE.to_owned(),
        group_admin: CORE.to_owned(),
        module_code: 2642,
        checksum: HexBinary::from_hex(OLD_CHECKSUM).unwrap(),
        block_start_weight: None,
        config,
        proposals: vec![ProposalRecord {
            id: 1,
            proposal: snapshot(env.block.height - 1, Status::Executed),
        }],
        messages: Default::default(),
    }));
    let querier_host = host.clone();
    let height = env.block.height;
    deps.querier.update_wasm(move |query| {
        let h = querier_host.borrow();
        if let Some(answer) = h.extra.as_ref().and_then(|extra| extra(query)) {
            return SystemResult::Ok(ContractResult::Ok(answer));
        }
        let response = match query {
            WasmQuery::ContractInfo { contract_addr } => {
                let mut info = ContractInfoResponse::default();
                info.code_id = match contract_addr.as_str() {
                    CORE => h.old_code,
                    PROPOSAL => h.module_code,
                    GROUP => 2628,
                    VOTING => 2643,
                    _ => panic!("unknown contract"),
                };
                info.admin = Some(h.core_admin.clone());
                to_json_binary(&info)
            }
            WasmQuery::CodeInfo { code_id } => {
                let mut info = CodeInfoResponse::default();
                info.code_id = *code_id;
                info.checksum = h.checksum.clone();
                to_json_binary(&info)
            }
            WasmQuery::Smart { contract_addr, msg } => match contract_addr.as_str() {
                VOTING => {
                    if let Ok(query) = from_json::<VotingQuery>(msg) {
                        to_json_binary(&Addr::unchecked(match query {
                            VotingQuery::Dao {} => CORE,
                            VotingQuery::GroupContract {} => GROUP,
                        }))
                    } else {
                        assert_eq!(
                            from_json::<voting::Query>(msg).unwrap(),
                            voting::Query::TotalPowerAtHeight {
                                height: Some(height)
                            }
                        );
                        to_json_binary(&voting::TotalPowerAtHeightResponse {
                            height,
                            power: Uint128::from(h.members.iter().map(|m| m.weight).sum::<u64>()),
                        })
                    }
                }
                GROUP => match from_json::<Cw4QueryMsg>(msg).unwrap() {
                    Cw4QueryMsg::Admin {} => to_json_binary(&AdminResponse {
                        admin: Some(h.group_admin.clone()),
                    }),
                    Cw4QueryMsg::ListMembers { limit, .. } => to_json_binary(&MemberListResponse {
                        members: h
                            .members
                            .iter()
                            .take(limit.unwrap_or(10) as usize)
                            .cloned()
                            .collect(),
                    }),
                    Cw4QueryMsg::TotalWeight { .. } => to_json_binary(&TotalWeightResponse {
                        weight: h.members.iter().map(|m| m.weight).sum(),
                    }),
                    Cw4QueryMsg::Member { addr, at_height } => to_json_binary(&MemberResponse {
                        weight: h.members.iter().find(|m| m.addr == addr).map(|m| {
                            if at_height.is_some() {
                                h.block_start_weight.unwrap_or(m.weight)
                            } else {
                                m.weight
                            }
                        }),
                    }),
                    _ => panic!("unexpected group query"),
                },
                PROPOSAL => match from_json::<ProposalQuery>(msg).unwrap() {
                    ProposalQuery::Config {} => to_json_binary(&h.config),
                    ProposalQuery::ProposalCount {} => to_json_binary(&h.proposals.len()),
                    ProposalQuery::ProposalCreationPolicy {} => {
                        to_json_binary(&ProposalCreationPolicy::Anyone {})
                    }
                    ProposalQuery::Proposal { proposal_id } => {
                        let record = h
                            .proposals
                            .iter()
                            .find(|p| p.id == proposal_id)
                            .expect("unknown proposal");
                        to_json_binary(&FullRecord {
                            id: record.id,
                            proposal: FullProposal {
                                start_height: record.proposal.start_height,
                                status: &record.proposal.status,
                                msgs: h.messages.get(&proposal_id).map_or(&[], Vec::as_slice),
                                #[cfg(feature = "phoenix-rotation")]
                                threshold: &record.proposal.threshold,
                                #[cfg(feature = "phoenix-rotation")]
                                veto: &record.proposal.veto,
                                #[cfg(feature = "phoenix-rotation")]
                                allow_revoting: record.proposal.allow_revoting,
                                #[cfg(feature = "phoenix-rotation")]
                                expiration: record.proposal.expiration,
                            },
                        })
                    }
                    ProposalQuery::ListProposals { start_after, limit } => {
                        to_json_binary(&ProposalList {
                            proposals: h
                                .proposals
                                .iter()
                                .filter(|p| p.id > start_after.unwrap_or_default())
                                .take(limit as usize)
                                .cloned()
                                .collect(),
                        })
                    }
                },
                _ => panic!("unknown smart query"),
            },
            _ => panic!("unexpected query"),
        }
        .unwrap();
        SystemResult::Ok(ContractResult::Ok(response))
    });
    (deps, env, host, payload)
}

#[test]
fn both_removals_set_absolute_three_and_preserve_unrelated_config() {
    for size in [4, 5] {
        let (mut deps, env, host, payload) = fixture(size);
        let response = migrate(deps.as_mut(), env.clone(), payload.clone()).unwrap();
        assert_eq!(response.messages.len(), 3);
        for (submsg, destination) in response.messages.iter().zip([GROUP, PROPOSAL]) {
            assert!(matches!(&submsg.msg,
                CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, funds, .. })
                if contract_addr == destination && funds.is_empty()
            ));
        }
        let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[0].msg else {
            panic!()
        };
        assert_eq!(
            from_json::<GroupExecute>(msg).unwrap(),
            GroupExecute::UpdateMembers {
                add: vec![],
                remove: vec!["fixture-member-0".to_owned()],
            }
        );
        let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[1].msg else {
            panic!()
        };
        let mut expected = host.borrow().config.clone();
        expected.threshold = Threshold::AbsoluteCount {
            threshold: Uint128::new(75),
        };
        assert_eq!(
            from_json::<ProposalExecute>(msg).unwrap(),
            ProposalExecute::UpdateConfig(expected)
        );
        assert_eq!(
            response.messages[2].msg,
            CosmosMsg::Wasm(WasmMsg::UpdateAdmin {
                contract_addr: CORE.to_owned(),
                admin: GOV.to_owned(),
            })
        );
        assert_eq!(ADMIN.load(&deps.storage).unwrap().as_str(), CORE);
        assert_eq!(
            get_contract_version(&deps.storage).unwrap().version,
            contract::CONTRACT_VERSION
        );
        assert!(migrate(deps.as_mut(), env, payload).is_err());
    }
}

#[test]
fn rejects_wrong_wasm_identity_admin_version_and_bindings() {
    for fail in 0..8 {
        let (mut deps, mut env, host, payload) = fixture(4);
        {
            let mut host = host.borrow_mut();
            match fail {
                0 => host.old_code = 999,
                1 => host.checksum = HexBinary::default(),
                2 => host.core_admin = GOV.to_owned(),
                3 => host.group_admin = GOV.to_owned(),
                4 => host.module_code = 999,
                5 => env.block.chain_id = "fixture-only".to_owned(),
                6 => set_contract_version(&mut deps.storage, contract::CONTRACT_NAME, "2.4.1")
                    .unwrap(),
                _ => NOMINATED_ADMIN
                    .save(&mut deps.storage, &Addr::unchecked("pending"))
                    .unwrap(),
            }
        }
        let before = get_contract_version(&deps.storage).unwrap();
        assert!(migrate(deps.as_mut(), env, payload).is_err(), "case {fail}");
        assert_eq!(get_contract_version(&deps.storage).unwrap(), before);
        assert!(BOUNDARY.may_load(&deps.storage).unwrap().is_none());
    }
}

#[test]
fn rejects_roster_weight_snapshot_or_config_drift() {
    for fail in 0..6 {
        let (mut deps, env, host, mut payload) = fixture(4);
        let MigrateMsg::AdoptPhoenix4856 {
            expected_members,
            outgoing,
            ..
        } = &mut payload;
        {
            let mut host = host.borrow_mut();
            match fail {
                0 => {
                    host.members.pop();
                }
                1 => host.members[0].weight = 26,
                2 => host.block_start_weight = Some(0),
                3 => host.config.allow_revoting = true,
                4 => expected_members[1] = expected_members[0].clone(),
                _ => *outgoing = "fixture-unreviewed".to_owned(),
            }
        }
        assert!(migrate(deps.as_mut(), env, payload).is_err(), "case {fail}");
    }
}

#[test]
fn rejects_actionable_legacy_and_changed_proposal_count() {
    for status in [
        Status::Open,
        Status::Passed,
        Status::VetoTimelock {
            expiration: cw_utils::Expiration::Never {},
        },
    ] {
        let (mut deps, env, host, payload) = fixture(4);
        host.borrow_mut().proposals[0].proposal.status = status;
        assert!(migrate(deps.as_mut(), env, payload)
            .unwrap_err()
            .to_string()
            .contains("resolve legacy proposal 1"));
    }
    let (mut deps, env, host, payload) = fixture(4);
    host.borrow_mut().proposals.clear();
    assert!(migrate(deps.as_mut(), env, payload)
        .unwrap_err()
        .to_string()
        .contains("proposal count is below the reviewed count"));
}

fn hook(deps: DepsMut, env: &Env, msgs: &[CosmosMsg]) -> StdResult<Response> {
    gate_execution(
        deps,
        env,
        &Addr::unchecked(PROPOSAL),
        Response::new().add_messages(msgs.to_vec()),
    )
}

fn fixture_msg(tag: &str) -> Vec<CosmosMsg> {
    vec![CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: format!("fixture-{tag}"),
        msg: Binary::default(),
        funds: vec![],
    })]
}

#[test]
fn guards_same_block_execution_and_first_new_proposal_snapshot() {
    let (mut deps, mut env, host, payload) = fixture(4);
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    let adoption_height = env.block.height;
    let msgs = fixture_msg("stale");
    assert!(hook(deps.as_mut(), &env, &msgs).is_err());
    env.block.height += 1;
    assert!(hook(deps.as_mut(), &env, &msgs).is_ok());
    host.borrow_mut().proposals.push(ProposalRecord {
        id: 2,
        proposal: snapshot(adoption_height, Status::Executed),
    });
    host.borrow_mut().messages.insert(2, msgs.clone());
    assert!(hook(deps.as_mut(), &env, &msgs)
        .unwrap_err()
        .to_string()
        .contains("proposal 2 predates the current membership"));
    host.borrow_mut().proposals[1].proposal.start_height = adoption_height + 1;
    assert!(hook(deps.as_mut(), &env, &msgs).is_ok());
}

#[test]
fn scans_all_pages_through_the_reviewed_proposal_count() {
    let (mut deps, env, host, mut payload) = fixture(4);
    let count = 307;
    host.borrow_mut().proposals = (1..=count)
        .map(|id| ProposalRecord {
            id,
            proposal: snapshot(1, Status::Executed),
        })
        .collect();
    let MigrateMsg::AdoptPhoenix4856 {
        expected_proposal_count,
        ..
    } = &mut payload;
    *expected_proposal_count = count;
    host.borrow_mut().proposals[105].proposal.status = Status::Passed;
    let failure = migrate(deps.as_mut(), env.clone(), payload.clone()).unwrap_err();
    assert!(failure.to_string().contains("legacy proposal 106"));
    host.borrow_mut().proposals[105].proposal.status = Status::Executed;
    assert!(migrate(deps.as_mut(), env, payload).is_ok());
}

#[test]
fn snapshot_projection_accepts_full_child_proposal_fields() {
    let response = br#"{"proposals":[{"id":7,"proposal":{"title":"Fixture","description":"Ignored","start_height":20,"status":"open","msgs":[],"total_power":"100","threshold":{"absolute_count":{"threshold":"75"}},"allow_revoting":false,"expiration":{"at_height":36}}}]}"#;
    let result: ProposalList = from_json(response).unwrap();
    assert_eq!(result.proposals[0].id, 7);
    assert_eq!(result.proposals[0].proposal.start_height, 20);
    assert_eq!(result.proposals[0].proposal.status, Status::Open);
}

#[test]
fn ordinary_execute_does_not_deserialize_rotation() {
    let serialized = Binary::from(br#"{"replace_board_member":{"outgoing":"fixture-old","incoming":"fixture-next","expected_weight":25}}"#.as_slice());
    assert!(from_json::<dao_interface::msg::ExecuteMsg>(serialized).is_err());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn send_activation_uses_full_delay_and_can_be_cancelled() {
    use cosmwasm_std::{coin, BankMsg};
    let (mut deps, mut env, _, payload) = fixture(4);
    let message = CosmosMsg::Bank(BankMsg::Send {
        to_address: "fixture-recipient".to_owned(),
        amount: vec![coin(1, "fixture")],
    });
    let send = SudoMsg::SendAsDao {
        messages: vec![message.clone()],
    };
    let enable = SudoMsg::SetSendAsDao { enabled: true };
    let disable = SudoMsg::SetSendAsDao { enabled: false };
    assert!(sudo(deps.as_mut(), env.clone(), enable.clone()).is_err());
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert!(sudo(deps.as_mut(), env.clone(), send.clone()).is_err());
    sudo(deps.as_mut(), env.clone(), enable.clone()).unwrap();
    env.block.time = env.block.time.plus_seconds(60);
    sudo(deps.as_mut(), env.clone(), disable.clone()).unwrap();
    sudo(deps.as_mut(), env.clone(), enable.clone()).unwrap();
    env.block.time = env.block.time.plus_seconds(48 * 60 * 60 - 1);
    assert!(sudo(deps.as_mut(), env.clone(), send.clone()).is_err());
    sudo(deps.as_mut(), env.clone(), enable).unwrap();
    env.block.time = env.block.time.plus_seconds(1);
    let response = sudo(deps.as_mut(), env.clone(), send.clone()).unwrap();
    assert_eq!(response.messages[0].msg, message);
    assert_eq!(
        sudo(deps.as_mut(), env.clone(), send.clone())
            .unwrap()
            .messages[0]
            .msg,
        message
    );
    sudo(deps.as_mut(), env.clone(), disable).unwrap();
    assert!(sudo(deps.as_mut(), env, send.clone()).is_err());
    assert!(from_json::<dao_interface::msg::ExecuteMsg>(to_json_binary(&send).unwrap()).is_err());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn migration_options_keep_admin_and_delay_send() {
    let (mut deps, mut env, _, mut payload) = fixture(4);
    let MigrateMsg::AdoptPhoenix4856 { governance, .. } = &mut payload;
    *governance = Some(GovernanceOptions {
        migration_admin: MigrationAdmin::KeepCurrentAdmin,
        allow_send_as_dao: true,
        seat: None,
        seat_veto_only: false,
        membership_changes: vec![MemberChange::Reweight {
            address: "fixture-member-1".to_owned(),
            weight: 26,
        }],
        veto: None,
    });
    let response = migrate(deps.as_mut(), env.clone(), payload).unwrap();
    assert_eq!(response.messages.len(), 2);
    let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &response.messages[0].msg else {
        panic!("expected group update");
    };
    assert_eq!(
        from_json::<GroupExecute>(msg).unwrap(),
        GroupExecute::UpdateMembers {
            add: vec![Member {
                addr: "fixture-member-1".to_owned(),
                weight: 26
            }],
            remove: vec!["fixture-member-0".to_owned()],
        }
    );
    let send = SudoMsg::SendAsDao {
        messages: vec![CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "fixture-contract".to_owned(),
            msg: Binary::default(),
            funds: vec![],
        })],
    };
    assert!(sudo(deps.as_mut(), env.clone(), send.clone()).is_err());
    env.block.time = env.block.time.plus_seconds(48 * 60 * 60);
    assert!(sudo(deps.as_mut(), env, send).is_ok());
}

#[test]
fn membership_changes_refuse_weights_that_make_two_directors_enough() {
    let deps = mock_dependencies();
    let roster = (1..=4)
        .map(|i| (format!("fixture-member-{i}"), 25))
        .collect();
    assert!(governance::plan_membership(
        deps.as_ref(),
        roster,
        vec![MemberChange::Reweight {
            address: "fixture-member-1".to_owned(),
            weight: 50
        }],
        None
    )
    .is_err());
}

#[cfg(feature = "phoenix-rotation")]
#[test]
fn sudo_rotation_updates_exactly_one_equal_weight_seat_even_while_paused() {
    let (mut deps, env, host, payload) = fixture(4);
    let rotation = SudoMsg::ReplaceBoardMember {
        outgoing: "fixture-member-1".to_owned(),
        incoming: "fixture-next".to_owned(),
        expected_weight: 25,
    };
    assert!(sudo(deps.as_mut(), env.clone(), rotation.clone()).is_err());
    migrate(deps.as_mut(), env.clone(), payload).unwrap();
    crate::state::PAUSED
        .save(&mut deps.storage, &cw_utils::Expiration::Never {})
        .unwrap();
    {
        let mut h = host.borrow_mut();
        h.core_admin = GOV.to_owned();
        h.members.remove(0);
        h.config.threshold = Threshold::AbsoluteCount {
            threshold: YES_WEIGHT,
        };
    }
    for (outgoing, incoming, expected_weight) in [
        ("fixture-member-0", "fixture-next", 25),
        ("fixture-member-1", "fixture-member-2", 25),
        ("fixture-member-1", "fixture-next", 24),
    ] {
        assert!(sudo(
            deps.as_mut(),
            env.clone(),
            SudoMsg::ReplaceBoardMember {
                outgoing: outgoing.to_owned(),
                incoming: incoming.to_owned(),
                expected_weight,
            }
        )
        .is_err());
    }
    let response = sudo(deps.as_mut(), env, rotation).unwrap();
    let [sub] = response.messages.as_slice() else {
        panic!("expected a single update")
    };
    let CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr,
        msg,
        funds,
    }) = &sub.msg
    else {
        panic!()
    };
    assert_eq!(contract_addr, GROUP);
    assert!(funds.is_empty());
    assert_eq!(
        from_json::<GroupExecute>(msg).unwrap(),
        GroupExecute::UpdateMembers {
            add: vec![Member {
                addr: "fixture-next".to_owned(),
                weight: 25
            }],
            remove: vec!["fixture-member-1".to_owned()],
        }
    );
}

mod controls;
mod gate;
