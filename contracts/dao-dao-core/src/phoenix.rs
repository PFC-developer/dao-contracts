use cosmwasm_schema::cw_serde;
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Addr, Deps, DepsMut, Env, HexBinary, Order, Response, StdError, StdResult,
    Uint128, WasmMsg,
};
use cw2::get_contract_version;
use cw4::{AdminResponse, Cw4QueryMsg, MemberListResponse, MemberResponse, TotalWeightResponse};
use cw4_group::msg::ExecuteMsg as GroupExecute;
use cw_storage_plus::Item;
use cw_utils::Duration;
use dao_interface::{msg::MigrateMsg as UpstreamMigrate, state::ProposalModuleStatus, voting};
use dao_voting::{
    pre_propose::ProposalCreationPolicy, status::Status, threshold::Threshold, veto::VetoConfig,
};

use crate::{
    contract,
    state::{
        ACTIVE_PROPOSAL_MODULE_COUNT, ADMIN, NOMINATED_ADMIN, PROPOSAL_MODULES, VOTING_MODULE,
    },
    ContractError,
};

mod governance;
#[cfg(feature = "phoenix-rotation")]
mod rotation;
pub use governance::{GovernanceOptions, GovernanceSeat, MemberChange, MigrationAdmin};
#[cfg(test)]
mod tests;
#[cfg(feature = "phoenix-rotation")]
pub use rotation::{sudo, SudoMsg};

const CORE: &str = "terra1k8ug6dkzntczfzn76wsh24tdjmx944yj6mk063wum7n20cwd7lxq4lppjg";
const GOV: &str = "terra10d07y265gmmuvt4z0w9aw880jnsr700juxf95n";
const PROPOSAL: &str = "terra1660g9mle5kfsq8c0p4k4hgr9ujdyr3m48c22cawy0akr98rmwksqehqnup";
const VOTING: &str = "terra15ghc7m0nqc6xmwwtwhdgrkhyr798sw9ueywthgdpgfx3eqfqcgys0nnh4d";
const GROUP: &str = "terra1pq9f4a8cn4cdvvm9j7v0mu8n5elf44tgtre8mzrfz3j4x6yyl2yq4hnkuk";
const OLD_CODE_ID: u64 = 2635;
const OLD_CHECKSUM: &str = "cf70918a88a9e0654e6b439b8927e8d7897bf113f25909973910682af227e287";
const SEAT_WEIGHT: u64 = 25;
const YES_WEIGHT: Uint128 = Uint128::new(3 * SEAT_WEIGHT as u128);
const MAX_PROPOSALS_TO_CHECK: u64 = 512;
const PROPOSALS_PER_QUERY: u64 = 12;

const BOUNDARY: Item<SnapshotBoundary> = Item::new("phoenix_snapshot_boundary_v1");

#[cw_serde]
pub enum MigrateMsg {
    AdoptPhoenix4856 {
        /// Complete, reviewed current roster. Every address must be a distinct 25-weight seat.
        expected_members: Vec<String>,
        /// One exact outgoing address from the implementation record.
        outgoing: String,
        expected_proposal_config: ProposalConfig,
        expected_creation_policy: ProposalCreationPolicy,
        /// Fail if any proposal was created after the review snapshot.
        expected_proposal_count: u64,
        /// Optional settings for the transition. Omitted values retain the original adoption path.
        governance: Option<GovernanceOptions>,
    },
}

/// The v2.4.0 single-choice module's Config response and UpdateConfig fields.
#[cw_serde]
pub struct ProposalConfig {
    pub threshold: Threshold,
    pub max_voting_period: Duration,
    pub min_voting_period: Option<Duration>,
    pub only_members_execute: bool,
    pub allow_revoting: bool,
    pub dao: String,
    pub close_proposal_on_execution_failure: bool,
    pub veto: Option<VetoConfig>,
}

#[cw_serde]
enum ProposalQuery {
    Config {},
    ProposalCreationPolicy {},
    ProposalCount {},
    ListProposals {
        start_after: Option<u64>,
        limit: u64,
    },
}

#[cw_serde]
enum ProposalExecute {
    UpdateConfig(ProposalConfig),
}

#[cw_serde]
enum VotingQuery {
    GroupContract {},
    Dao {},
}

// The deployed proposal query includes human text, messages, and vote counts.
// These projections decode only the fields needed to gate the transition.
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct ProposalList {
    proposals: Vec<ProposalRecord>,
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(Clone, serde::Serialize))]
struct ProposalRecord {
    id: u64,
    proposal: ProposalSnapshot,
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(Clone, serde::Serialize))]
struct ProposalSnapshot {
    start_height: u64,
    status: Status,
}

#[cw_serde]
struct SnapshotBoundary {
    height: u64,
    proposal_count: u64,
}

fn require(condition: bool, message: impl Into<String>) -> StdResult<()> {
    if condition {
        Ok(())
    } else {
        Err(StdError::generic_err(message))
    }
}

fn check_binding(deps: Deps, env: &Env) -> StdResult<()> {
    governance::check_context(env)?;
    require(
        VOTING_MODULE.load(deps.storage)?.as_str() == VOTING
            && deps.querier.query_wasm_contract_info(VOTING)?.code_id == 2643,
        "unexpected voting module or code",
    )?;
    let dao: Addr = deps
        .querier
        .query_wasm_smart(VOTING, &VotingQuery::Dao {})?;
    let group: Addr = deps
        .querier
        .query_wasm_smart(VOTING, &VotingQuery::GroupContract {})?;
    require(
        dao.as_str() == CORE
            && group.as_str() == GROUP
            && deps.querier.query_wasm_contract_info(GROUP)?.code_id == 2628,
        "unexpected voting DAO, membership group or code",
    )?;
    let admin: AdminResponse = deps
        .querier
        .query_wasm_smart(GROUP, &Cw4QueryMsg::Admin {})?;
    require(
        admin.admin.as_deref() == Some(CORE),
        "core must administer group membership",
    )
}

fn check_proposal_module(deps: Deps) -> StdResult<()> {
    let enabled = PROPOSAL_MODULES
        .range(deps.storage, None, None, Order::Ascending)
        .filter_map(|result| match result {
            Ok((_, module)) if module.status == ProposalModuleStatus::Disabled => None,
            other => Some(other),
        })
        .take(2)
        .collect::<StdResult<Vec<_>>>()?;
    require(
        matches!(enabled.as_slice(), [(key, module)]
            if key.as_str() == PROPOSAL && module.address == *key && module.prefix == "A")
            && ACTIVE_PROPOSAL_MODULE_COUNT.load(deps.storage)? == 1
            && deps.querier.query_wasm_contract_info(PROPOSAL)?.code_id == 2642,
        "unexpected active proposal modules or code",
    )
}

fn check_roster(deps: Deps, height: u64, members: &[String], outgoing: &str) -> StdResult<()> {
    require(
        matches!(members.len(), 4 | 5),
        "expected four or five reviewed members before removal",
    )?;
    let mut expected = members
        .iter()
        .map(|addr| deps.api.addr_validate(addr).map(Addr::into_string))
        .collect::<StdResult<Vec<_>>>()?;
    expected.sort_unstable();
    require(
        !expected.windows(2).any(|pair| pair[0] == pair[1]),
        "duplicate member in reviewed roster",
    )?;
    require(
        expected.iter().any(|member| member == outgoing),
        "outgoing address absent from reviewed roster",
    )?;
    let actual: MemberListResponse = deps.querier.query_wasm_smart(
        GROUP,
        &Cw4QueryMsg::ListMembers {
            start_after: None,
            limit: Some(6),
        },
    )?;
    let mut actual = actual.members;
    actual.sort_unstable_by(|a, b| a.addr.cmp(&b.addr));
    require(
        actual.len() == expected.len()
            && actual
                .iter()
                .zip(&expected)
                .all(|(a, e)| a.addr == *e && a.weight == SEAT_WEIGHT),
        "current roster or weights changed",
    )?;
    let total: TotalWeightResponse = deps.querier.query_wasm_smart(
        GROUP,
        &Cw4QueryMsg::TotalWeight {
            at_height: Some(height),
        },
    )?;
    require(
        total.weight == expected.len() as u64 * SEAT_WEIGHT,
        "block-start total changed",
    )?;
    for addr in expected {
        let member: MemberResponse = deps.querier.query_wasm_smart(
            GROUP,
            &Cw4QueryMsg::Member {
                addr,
                at_height: Some(height),
            },
        )?;
        require(
            member.weight == Some(SEAT_WEIGHT),
            "block-start member changed",
        )?;
    }
    Ok(())
}

fn check_drained_proposals(deps: Deps, mut checked: u64, expected: u64) -> StdResult<()> {
    let count: u64 = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::ProposalCount {})?;
    require(count == expected, "proposal count changed since review")?;
    require(
        count
            .checked_sub(checked)
            .is_some_and(|n| n <= MAX_PROPOSALS_TO_CHECK),
        "proposal count exceeds checked bound",
    )?;
    while checked < count {
        let limit = (count - checked).min(PROPOSALS_PER_QUERY);
        let page: ProposalList = deps.querier.query_wasm_smart(
            PROPOSAL,
            &ProposalQuery::ListProposals {
                start_after: Some(checked),
                limit,
            },
        )?;
        require(
            page.proposals.len() as u64 == limit,
            "incomplete proposal page",
        )?;
        for record in page.proposals {
            checked += 1;
            require(record.id == checked, "proposal history gap")?;
            require(
                matches!(
                    record.proposal.status,
                    Status::Rejected
                        | Status::Closed
                        | Status::Executed
                        | Status::ExecutionFailed
                        | Status::Vetoed
                ),
                format!("resolve legacy proposal {} before adoption", record.id),
            )?;
        }
    }
    Ok(())
}

// v2.4.0's execution hook contains no proposal ID. Check the first proposal
// after the reviewed range to detect any creation at the mutation height.
// The source module assigns contiguous IDs in increasing chain height order.
pub(crate) fn check_snapshot_boundary(deps: Deps, env: &Env) -> StdResult<()> {
    let Some(boundary) = BOUNDARY.may_load(deps.storage)? else {
        return Ok(());
    };
    require(
        env.block.height > boundary.height,
        "proposal execution blocked at adoption height",
    )?;
    let page: ProposalList = deps.querier.query_wasm_smart(
        PROPOSAL,
        &ProposalQuery::ListProposals {
            start_after: Some(boundary.proposal_count),
            limit: 1,
        },
    )?;
    if let Some(first) = page.proposals.first() {
        require(
            first.proposal.start_height > boundary.height,
            "proposal created at adoption height; governance review required",
        )?;
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, env: Env, msg: MigrateMsg) -> Result<Response, ContractError> {
    check_binding(deps.as_ref(), &env)?;
    let old = get_contract_version(deps.storage)?;
    require(
        old.contract == contract::CONTRACT_NAME
            && old.version == "2.4.0"
            && BOUNDARY.may_load(deps.storage)?.is_none(),
        "expected unmigrated upstream dao-dao-core 2.4.0",
    )?;
    let info = deps.querier.query_wasm_contract_info(CORE)?;
    // Wasmd calls the entry point before persisting the new code ID.
    require(
        info.code_id == OLD_CODE_ID
            && info.admin.as_deref() == Some(CORE)
            && ADMIN.load(deps.storage)?.as_str() == CORE
            && NOMINATED_ADMIN.may_load(deps.storage)?.is_none(),
        "unexpected predecessor code, core admin or nomination",
    )?;
    let code = deps.querier.query_wasm_code_info(OLD_CODE_ID)?;
    require(
        code.checksum == HexBinary::from_hex(OLD_CHECKSUM)?,
        "unexpected predecessor checksum",
    )?;
    check_proposal_module(deps.as_ref())?;
    let MigrateMsg::AdoptPhoenix4856 {
        expected_members,
        outgoing,
        expected_proposal_config,
        expected_creation_policy,
        expected_proposal_count,
        governance,
    } = msg;
    let outgoing = deps.api.addr_validate(&outgoing)?.into_string();
    check_roster(
        deps.as_ref(),
        env.block.height,
        &expected_members,
        &outgoing,
    )?;
    let mut cfg: ProposalConfig = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Config {})?;
    let policy: ProposalCreationPolicy = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::ProposalCreationPolicy {})?;
    require(
        cfg.dao == CORE && cfg == expected_proposal_config && policy == expected_creation_policy,
        "proposal config or creation policy changed",
    )?;
    // Verify the voting module sees the same total power at proposal snapshot height.
    let voting: voting::TotalPowerAtHeightResponse = deps.querier.query_wasm_smart(
        VOTING,
        &voting::Query::TotalPowerAtHeight {
            height: Some(env.block.height),
        },
    )?;
    require(
        voting.height == env.block.height
            && voting.power == Uint128::from(expected_members.len() as u64 * SEAT_WEIGHT),
        "unexpected voting power",
    )?;
    check_drained_proposals(deps.as_ref(), 0, expected_proposal_count)?;
    let mut changes = vec![MemberChange::Remove {
        address: outgoing.clone(),
    }];
    let mut transfer_admin = true;
    let mut allow_send = false;
    let mut seat = None;
    if let Some(opts) = governance {
        require(
            cfg!(feature = "phoenix-rotation") || (opts.seat.is_none() && !opts.allow_send_as_dao),
            "standing governance controls are not enabled in this build",
        )?;
        transfer_admin = opts.migration_admin == MigrationAdmin::GovernanceOnly;
        allow_send = opts.allow_send_as_dao;
        changes.extend(opts.membership_changes);
        if let Some(veto) = opts.veto {
            require(
                veto.vetoer == GOV || opts.seat.as_ref().is_some_and(|s| s.address == veto.vetoer),
                "expected governance vetoer",
            )?;
            cfg.veto = Some(veto);
        }
        if let Some(approved) = opts.seat {
            governance::check_seat(deps.as_ref(), &approved)?;
            changes.push(MemberChange::Add {
                address: approved.address.clone(),
                weight: SEAT_WEIGHT,
            });
            seat = Some(approved);
        }
    }
    let previous = expected_members
        .iter()
        .cloned()
        .map(|addr| (addr, SEAT_WEIGHT))
        .collect();
    let (membership, director_count) =
        governance::plan_membership(deps.as_ref(), previous, changes, seat.as_ref())?;
    BOUNDARY.save(
        deps.storage,
        &SnapshotBoundary {
            height: env.block.height,
            proposal_count: expected_proposal_count,
        },
    )?;
    if let Some(seat) = seat {
        governance::SEAT.save(deps.storage, &seat)?;
    }
    let send = governance::set_send_enabled(deps.storage, env.block.time, allow_send)?;
    cfg.threshold = Threshold::AbsoluteCount {
        threshold: YES_WEIGHT,
    };
    let response = contract::migrate(deps, env, UpstreamMigrate::FromCompatible {})?;
    let response = response
        .add_message(WasmMsg::Execute {
            contract_addr: GROUP.to_string(),
            msg: to_json_binary(&membership)?,
            funds: vec![],
        })
        .add_message(WasmMsg::Execute {
            contract_addr: PROPOSAL.to_string(),
            msg: to_json_binary(&ProposalExecute::UpdateConfig(cfg))?,
            funds: vec![],
        })
        .add_attribute("action", "adopt_phoenix_4856")
        .add_attribute("removed", outgoing)
        .add_attribute("remaining_seats", director_count.to_string())
        .add_attribute("yes_weight_required", YES_WEIGHT)
        .add_attributes(
            send.attributes
                .into_iter()
                .filter(|attr| attr.key != "action"),
        );
    Ok(if transfer_admin {
        response.add_message(WasmMsg::UpdateAdmin {
            contract_addr: CORE.to_string(),
            admin: GOV.to_string(),
        })
    } else {
        response
    })
}
