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
    /// Classify up to 48 proposals toward the next membership change.
    /// Repeat until the reported cursor has caught up with the proposal count.
    CheckpointProposalHistory {},
    SetSendAsDao {
        enabled: bool,
    },
    /// Approve the stored seat's current code, or replace or clear a seat
    /// that is not a group member. Membership actions seat and unseat it.
    SetGovernanceSeat {
        seat: Option<GovernanceSeat>,
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
pub fn sudo(mut deps: DepsMut, env: Env, msg: SudoMsg) -> Result<Response, ContractError> {
    governance::check_context(&env)?;
    let boundary = BOUNDARY.load(deps.storage)?;
    match msg {
        SudoMsg::CheckpointProposalHistory {} => {
            // Trust the pinned module's terminal statuses even when its core
            // registration needs to be repaired by the next membership action.
            require(
                deps.querier.query_wasm_contract_info(PROPOSAL)?.code_id == 2642,
                "unexpected proposal module code",
            )?;
            Ok(checkpoint_history(deps, &boundary)?)
        }
        SudoMsg::SetSendAsDao { enabled } => Ok(governance::set_send_enabled(
            deps.storage,
            env.block.time,
            enabled,
        )?),
        SudoMsg::SetGovernanceSeat { seat } => Ok(governance::set_seat(deps, seat)?),
        SudoMsg::SendAsDao { messages } => Ok(governance::send_as_dao(
            deps.as_ref(),
            env.block.time,
            messages,
        )?),
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
            Ok(response.add_message(WasmMsg::UpdateAdmin {
                contract_addr: CORE.to_owned(),
                admin,
            }))
        }
        SudoMsg::UpdateMembership {
            expected_members,
            expected_proposal_count,
            changes,
        } => {
            let restored = restore_membership_binding(deps.branch(), &env)?;
            let roster =
                governance::reviewed_roster(deps.as_ref(), env.block.height, &expected_members)?;
            let seat = governance::SEAT.may_load(deps.storage)?;
            let (membership, _) =
                governance::plan_membership(deps.as_ref(), roster, changes, seat.as_ref())?;
            let response =
                update_membership(deps, env, boundary, expected_proposal_count, membership)?;
            Ok(with_restored(response, restored))
        }
        SudoMsg::ReplaceBoardMember {
            outgoing,
            incoming,
            expected_weight,
        } => {
            let restored = restore_membership_binding(deps.branch(), &env)?;
            let response =
                replace_member(deps, env, boundary, outgoing, incoming, expected_weight)?;
            Ok(with_restored(response, restored))
        }
    }
}

/// Membership actions must not depend on settings the DAO can change with an
/// ordinary proposal. Restore the ones this core controls and report them.
/// Contract code, the group admin and the module's DAO cannot be restored
/// from here, so a change to any of them still fails.
fn restore_membership_binding(deps: DepsMut, env: &Env) -> StdResult<Response> {
    check_binding(deps.as_ref(), env)?;
    let primary = Addr::unchecked(PROPOSAL);
    let module = PROPOSAL_MODULES.may_load(deps.storage, primary.clone())?;
    require(
        module
            .as_ref()
            .is_some_and(|m| m.address == primary && m.prefix == "A")
            && deps.querier.query_wasm_contract_info(PROPOSAL)?.code_id == 2642,
        "unexpected proposal module or code",
    )?;
    let mut response = Response::new();
    let modules = PROPOSAL_MODULES
        .range(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;
    let mut changed = 0u32;
    for (key, mut module) in modules {
        let expected = if key == primary {
            ProposalModuleStatus::Enabled
        } else {
            ProposalModuleStatus::Disabled
        };
        if module.status != expected {
            module.status = expected;
            PROPOSAL_MODULES.save(deps.storage, key, &module)?;
            changed += 1;
        }
    }
    if changed > 0 || ACTIVE_PROPOSAL_MODULE_COUNT.load(deps.storage)? != 1 {
        ACTIVE_PROPOSAL_MODULE_COUNT.save(deps.storage, &1)?;
        response = response.add_attribute("restored_proposal_modules", changed.to_string());
    }
    let mut cfg: ProposalConfig = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Config {})?;
    require(cfg.dao == CORE, "proposal module DAO changed")?;
    let threshold = Threshold::AbsoluteCount {
        threshold: YES_WEIGHT,
    };
    if cfg.threshold != threshold {
        cfg.threshold = threshold;
        response = response
            .add_message(WasmMsg::Execute {
                contract_addr: PROPOSAL.to_owned(),
                msg: to_json_binary(&ProposalExecute::UpdateConfig(cfg))?,
                funds: vec![],
            })
            .add_attribute("restored_threshold", YES_WEIGHT);
    }
    Ok(response)
}

fn with_restored(response: Response, restored: Response) -> Response {
    response
        .add_submessages(restored.messages)
        .add_attributes(restored.attributes)
}

pub(super) fn check_membership_binding(deps: Deps, env: &Env) -> StdResult<ProposalConfig> {
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
    )?;
    Ok(cfg)
}

fn replace_member(
    deps: DepsMut,
    env: Env,
    boundary: SnapshotBoundary,
    outgoing: String,
    incoming: String,
    expected_weight: u64,
) -> Result<Response, ContractError> {
    let outgoing = deps.api.addr_validate(&outgoing)?.into_string();
    let incoming = deps.api.addr_validate(&incoming)?.into_string();
    let roster = governance::current_members(deps.as_ref(), governance::MAX_MEMBERS)?;
    require(
        expected_weight == SEAT_WEIGHT
            && roster
                .iter()
                .any(|m| m.addr == outgoing && m.weight == expected_weight),
        "outgoing seat or expected weight changed",
    )?;
    let seat = governance::SEAT.may_load(deps.storage)?;
    let roster = governance::reviewed_roster(deps.as_ref(), env.block.height, &roster)?;
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
    // No count was reviewed for a replacement. Every proposal since the last
    // boundary that can still pass or execute is blocked.
    let reviewed = boundary.proposal_count;
    Ok(
        update_membership(deps, env, boundary, reviewed, membership)?
            .add_attribute("outgoing", outgoing)
            .add_attribute("incoming", incoming)
            .add_attribute("weight", expected_weight.to_string()),
    )
}

fn update_membership(
    deps: DepsMut,
    env: Env,
    boundary: SnapshotBoundary,
    reviewed: u64,
    membership: GroupExecute,
) -> Result<Response, ContractError> {
    let gated = plan_boundary(deps.as_ref(), boundary.proposal_count, reviewed)?
        .save(deps.storage, env.block.height)?;
    let response = Response::new()
        .add_message(WasmMsg::Execute {
            contract_addr: GROUP.to_owned(),
            msg: to_json_binary(&membership)?,
            funds: vec![],
        })
        .add_attribute("action", "update_governance_membership");
    Ok(gated_attribute(response, &gated))
}
