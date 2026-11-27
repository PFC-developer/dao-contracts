use cosmwasm_std::{CosmosMsg, Empty};
use cw4::Member;

use super::*;

#[cw_serde]
pub enum SudoMsg {
    ReplaceBoardMember {
        outgoing: String,
        incoming: String,
        expected_weight: u64,
    },
    UpdateMembership {
        expected_members: Vec<Member>,
        expected_proposal_count: u64,
        changes: Vec<MemberChange>,
    },
    SetSendAsDao {
        enabled: bool,
    },
    /// Change the host admin while this core is its own admin.
    /// A different current admin requires a native host admin message.
    SetMigrationAdmin {
        admin: String,
    },
    SendAsDao {
        messages: Vec<CosmosMsg<Empty>>,
    },
}

/// Wasmd's native governance handler is responsible for authorizing the call.
/// This export deliberately does not share the ordinary core pause path.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn sudo(deps: DepsMut, env: Env, msg: SudoMsg) -> Result<Response, ContractError> {
    governance::check_context(&env)?;
    let boundary = BOUNDARY.load(deps.storage)?;
    match msg {
        SudoMsg::SetSendAsDao { enabled } => {
            return Ok(governance::set_send_enabled(
                deps.storage,
                env.block.time,
                enabled,
            )?);
        }
        SudoMsg::SendAsDao { messages } => {
            return Ok(governance::send_as_dao(
                deps.as_ref(),
                env.block.time,
                messages,
            )?);
        }
        SudoMsg::SetMigrationAdmin { admin } => {
            let admin = deps.api.addr_validate(&admin)?.into_string();
            let current = deps.querier.query_wasm_contract_info(CORE)?.admin;
            let response = Response::new()
                .add_attribute("action", "set_governance_migration_admin")
                .add_attribute("admin", &admin);
            if current.as_deref() == Some(admin.as_str()) {
                return Ok(response);
            }
            require(
                current.as_deref() == Some(CORE),
                "current host admin must use a native admin message",
            )?;
            return Ok(response.add_message(WasmMsg::UpdateAdmin {
                contract_addr: CORE.to_owned(),
                admin,
            }));
        }
        SudoMsg::UpdateMembership {
            expected_members,
            expected_proposal_count,
            changes,
        } => {
            check_membership_binding(deps.as_ref(), &env)?;
            let roster =
                governance::reviewed_roster(deps.as_ref(), env.block.height, &expected_members)?;
            let seat = governance::SEAT.may_load(deps.storage)?;
            if let Some(seat) = &seat {
                governance::check_seat(deps.as_ref(), seat)?;
            }
            let (membership, _) =
                governance::plan_membership(deps.as_ref(), roster, changes, seat.as_ref())?;
            return update_membership(deps, env, boundary, expected_proposal_count, membership);
        }
        SudoMsg::ReplaceBoardMember {
            outgoing,
            incoming,
            expected_weight,
        } => replace_member(deps, env, boundary, outgoing, incoming, expected_weight),
    }
}

fn check_membership_binding(deps: Deps, env: &Env) -> StdResult<()> {
    check_binding(deps, env)?;
    check_proposal_module(deps)?;
    let cfg: ProposalConfig = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Config {})?;
    require(
        cfg.dao == CORE
            && cfg.threshold
                == (Threshold::AbsoluteCount {
                    threshold: YES_WEIGHT,
                }),
        "proposal threshold or DAO changed",
    )
}

fn replace_member(
    deps: DepsMut,
    env: Env,
    boundary: SnapshotBoundary,
    outgoing: String,
    incoming: String,
    expected_weight: u64,
) -> Result<Response, ContractError> {
    check_membership_binding(deps.as_ref(), &env)?;
    let outgoing = deps.api.addr_validate(&outgoing)?.into_string();
    let incoming = deps.api.addr_validate(&incoming)?.into_string();
    let roster: MemberListResponse = deps.querier.query_wasm_smart(
        GROUP,
        &Cw4QueryMsg::ListMembers {
            start_after: None,
            limit: Some(7),
        },
    )?;
    require(
        expected_weight == SEAT_WEIGHT
            && roster
                .members
                .iter()
                .any(|m| m.addr == outgoing && m.weight == expected_weight),
        "outgoing seat or expected weight changed",
    )?;
    let seat = governance::SEAT.may_load(deps.storage)?;
    if let Some(seat) = &seat {
        governance::check_seat(deps.as_ref(), seat)?;
    }
    let roster = governance::reviewed_roster(deps.as_ref(), env.block.height, &roster.members)?;
    let (membership, _) = governance::plan_membership(
        deps.as_ref(),
        roster,
        vec![
            MemberChange::Remove {
                address: outgoing.clone(),
            },
            MemberChange::Add {
                address: incoming.clone(),
                weight: expected_weight,
            },
        ],
        seat.as_ref(),
    )?;
    let count = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::ProposalCount {})?;
    Ok(update_membership(deps, env, boundary, count, membership)?
        .add_attribute("outgoing", outgoing)
        .add_attribute("incoming", incoming)
        .add_attribute("weight", expected_weight.to_string()))
}

fn update_membership(
    deps: DepsMut,
    env: Env,
    boundary: SnapshotBoundary,
    count: u64,
    membership: GroupExecute,
) -> Result<Response, ContractError> {
    check_drained_proposals(deps.as_ref(), boundary.proposal_count, count)?;
    BOUNDARY.save(
        deps.storage,
        &SnapshotBoundary {
            height: env.block.height,
            proposal_count: count,
        },
    )?;
    Ok(Response::new()
        .add_message(WasmMsg::Execute {
            contract_addr: GROUP.to_owned(),
            msg: to_json_binary(&membership)?,
            funds: vec![],
        })
        .add_attribute("action", "update_governance_membership"))
}
