use cosmwasm_schema::cw_serde;
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Addr, CosmosMsg, Deps, DepsMut, Empty, Env, HexBinary, Order, Response,
    StdError, StdResult, Storage, SubMsg, Uint128, WasmMsg,
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
mod host;
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
#[cfg(feature = "phoenix-rotation")]
const PROPOSALS_PER_CHECKPOINT: u64 = 4 * PROPOSALS_PER_QUERY;

const MAX_GATED: usize = 64;
#[cfg(feature = "phoenix-rotation")]
const GUARD_REPLY_ID: u64 = 4856;

const BOUNDARY: Item<SnapshotBoundary> = Item::new("phoenix_snapshot_boundary_v1");
/// Proposals that were unresolved when a boundary was set. They snapshot an
/// earlier roster, so the core refuses to dispatch their messages.
const GATED: Item<Vec<u64>> = Item::new("phoenix_gated_proposals_v1");
#[cfg(feature = "phoenix-rotation")]
const PENDING_HISTORY: Item<PendingHistory> = Item::new("phoenix_pending_history_v1");
#[cfg(feature = "phoenix-rotation")]
const GUARDED_BATCH: Item<GuardedBatch> = Item::new("phoenix_guarded_batch_v1");

/// This contract is built in two forms: the default build, and the build with
/// the `phoenix-rotation` feature, which adds the host-authorized governance
/// controls. They are alternative adoption targets. Each accepts only the
/// unmigrated upstream 2.4.0 core as its predecessor, so a core adopted with
/// the default build cannot migrate to the governance build. Adding the
/// controls later needs a new, separately reviewed build.
#[cw_serde]
pub enum MigrateMsg {
    AdoptPhoenix4856 {
        /// Complete, reviewed current roster. Every address must be a distinct 25-weight seat.
        expected_members: Vec<String>,
        /// One exact outgoing address from the implementation record.
        outgoing: String,
        expected_proposal_config: ProposalConfig,
        expected_creation_policy: ProposalCreationPolicy,
        /// Proposal count at the review snapshot. Every proposal up to it must be
        /// resolved. Later proposals do not stop adoption, but any of them still
        /// unresolved at adoption is permanently blocked from executing.
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
    Proposal {
        proposal_id: u64,
    },
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
    #[cfg(feature = "phoenix-rotation")]
    threshold: Threshold,
    #[cfg(feature = "phoenix-rotation")]
    veto: Option<VetoConfig>,
    #[cfg(feature = "phoenix-rotation")]
    allow_revoting: bool,
    #[cfg(feature = "phoenix-rotation")]
    expiration: cw_utils::Expiration,
}

// Decoded only for a proposal the module reports as executed.
#[derive(serde::Deserialize)]
struct ExecutedRecord {
    proposal: ExecutedSnapshot,
}

#[derive(serde::Deserialize)]
struct ExecutedSnapshot {
    msgs: Vec<CosmosMsg<Empty>>,
}

/// `proposal_count` is a cursor. Every proposal up to it has been classified
/// as resolved or recorded in `GATED`.
#[cw_serde]
struct SnapshotBoundary {
    height: u64,
    proposal_count: u64,
}

/// Tracks work toward the next boundary without changing execution eligibility.
#[cfg(feature = "phoenix-rotation")]
#[cw_serde]
struct PendingHistory {
    through: u64,
    unresolved: Vec<u64>,
}

#[cfg(feature = "phoenix-rotation")]
#[cw_serde]
struct GuardedBatch {
    proposal_count: u64,
    veto: Option<VetoConfig>,
    members: Vec<cw4::Member>,
    seated_voting_period: Option<Duration>,
}

struct PlannedBoundary {
    proposal_count: u64,
    gated: Vec<u64>,
    added: usize,
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

fn load_proposal(deps: Deps, id: u64) -> StdResult<ProposalSnapshot> {
    let record: ProposalRecord = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Proposal { proposal_id: id })?;
    require(record.id == id, "unexpected proposal record")?;
    Ok(record.proposal)
}

fn is_resolved(status: &Status) -> bool {
    matches!(
        status,
        Status::Rejected
            | Status::Closed
            | Status::Executed
            | Status::ExecutionFailed
            | Status::Vetoed
    )
}

fn proposal_count(deps: Deps) -> StdResult<u64> {
    deps.querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::ProposalCount {})
}

fn visit_proposals(
    deps: Deps,
    mut checked: u64,
    end: u64,
    mut visit: impl FnMut(ProposalRecord) -> StdResult<()>,
) -> StdResult<()> {
    while checked < end {
        let limit = (end - checked).min(PROPOSALS_PER_QUERY);
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
            visit(record)?;
        }
    }
    Ok(())
}

fn keep_unresolved(deps: Deps, ids: impl IntoIterator<Item = u64>) -> StdResult<Vec<u64>> {
    let mut unresolved = Vec::new();
    for id in ids {
        if !is_resolved(&load_proposal(deps, id)?.status) {
            unresolved.push(id);
        }
    }
    Ok(unresolved)
}

#[cfg(feature = "phoenix-rotation")]
fn pending_history(deps: Deps, checked: u64) -> StdResult<PendingHistory> {
    let history = PENDING_HISTORY
        .may_load(deps.storage)?
        .unwrap_or(PendingHistory {
            through: checked,
            unresolved: vec![],
        });
    // The execution hook can separately classify the boundary-height tail.
    Ok(PendingHistory {
        through: history.through.max(checked),
        unresolved: keep_unresolved(
            deps,
            history.unresolved.into_iter().filter(|&id| id > checked),
        )?,
    })
}

#[cfg(feature = "phoenix-rotation")]
fn checkpoint_history(deps: DepsMut, boundary: &SnapshotBoundary) -> StdResult<Response> {
    let mut history = pending_history(deps.as_ref(), boundary.proposal_count)?;
    let count = proposal_count(deps.as_ref())?;
    require(count >= history.through, "proposal count moved backwards")?;
    let end = count.min(history.through.saturating_add(PROPOSALS_PER_CHECKPOINT));
    visit_proposals(deps.as_ref(), history.through, end, |record| {
        if !is_resolved(&record.proposal.status) {
            history.unresolved.push(record.id);
        }
        Ok(())
    })?;
    require(
        history.unresolved.len() <= MAX_GATED,
        "too many unresolved proposals to checkpoint",
    )?;
    history.through = end;
    PENDING_HISTORY.save(deps.storage, &history)?;
    Ok(Response::new()
        .add_attribute("action", "checkpoint_proposal_history")
        .add_attribute("checked_through", end.to_string())
        .add_attribute("proposal_count", count.to_string()))
}

// Classify proposals since the last boundary. Those up to the reviewed ID
// must be resolved. Any later unresolved proposals are gated instead.
fn plan_boundary(deps: Deps, checked: u64, reviewed: u64) -> StdResult<PlannedBoundary> {
    let count = proposal_count(deps)?;
    require(
        count >= reviewed,
        "proposal count is below the reviewed count",
    )?;
    #[cfg(feature = "phoenix-rotation")]
    let (checked, staged) = {
        let history = pending_history(deps, checked)?;
        (history.through, history.unresolved)
    };
    #[cfg(not(feature = "phoenix-rotation"))]
    let staged: Vec<u64> = vec![];
    require(
        count
            .checked_sub(checked)
            .is_some_and(|n| n <= MAX_PROPOSALS_TO_CHECK),
        "proposal count exceeds checked bound",
    )?;
    // This runs outside proposal execution or after the executing proposal has
    // passed the existing gate, so an executed gated entry is settled.
    let mut gated = keep_unresolved(deps, GATED.may_load(deps.storage)?.unwrap_or_default())?;
    let carried = gated.len();
    let mut gate = |id| -> StdResult<()> {
        require(
            id > reviewed,
            format!("resolve legacy proposal {id} before changing membership"),
        )?;
        gated.push(id);
        Ok(())
    };
    for id in staged {
        gate(id)?;
    }
    visit_proposals(deps, checked, count, |record| {
        if !is_resolved(&record.proposal.status) {
            gate(record.id)?;
        }
        Ok(())
    })?;
    require(
        gated.len() <= MAX_GATED,
        "too many unresolved proposals to block",
    )?;
    Ok(PlannedBoundary {
        proposal_count: count,
        added: gated.len() - carried,
        gated,
    })
}

impl PlannedBoundary {
    /// Record the boundary and return the proposals it newly blocks.
    fn save(mut self, storage: &mut dyn Storage, height: u64) -> StdResult<Vec<u64>> {
        #[cfg(feature = "phoenix-rotation")]
        PENDING_HISTORY.remove(storage);
        GATED.save(storage, &self.gated)?;
        BOUNDARY.save(
            storage,
            &SnapshotBoundary {
                height,
                proposal_count: self.proposal_count,
            },
        )?;
        Ok(self.gated.split_off(self.gated.len() - self.added))
    }
}

fn gated_attribute(response: Response, gated: &[u64]) -> Response {
    if gated.is_empty() {
        return response;
    }
    let ids: Vec<_> = gated.iter().map(u64::to_string).collect();
    response.add_attribute("gated_proposals", ids.join(","))
}

// Whether a proposal that predates the boundary must stay gated. Fails when it
// is the proposal whose messages are being dispatched now.
//
// v2.4.0's execution hook contains no proposal ID. The module marks a proposal
// executed before it calls the hook with that proposal's messages, and marks a
// proposal with no messages executed without calling the core at all. An
// executed proposal with other messages is therefore settled, not running.
fn stays_gated(deps: Deps, id: u64, status: &Status, hook: &[SubMsg]) -> StdResult<bool> {
    match status {
        Status::Rejected | Status::Closed | Status::ExecutionFailed | Status::Vetoed => Ok(false),
        Status::Executed => {
            let record: ExecutedRecord = deps
                .querier
                .query_wasm_smart(PROPOSAL, &ProposalQuery::Proposal { proposal_id: id })?;
            let running = record.proposal.msgs.iter().eq(hook.iter().map(|m| &m.msg));
            require(
                !running,
                format!("proposal {id} predates the current membership and cannot execute"),
            )?;
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Runs on every proposal execution hook, before its messages are dispatched.
///
/// Refuses a proposal that was unresolved when the boundary was set, or that
/// was created at the boundary height after it. Other proposals execute
/// normally. The source module assigns contiguous IDs in increasing chain
/// height order.
pub(crate) fn gate_execution(
    deps: DepsMut,
    env: &Env,
    sender: &Addr,
    response: Response,
) -> StdResult<Response> {
    let Some(boundary) = BOUNDARY.may_load(deps.storage)? else {
        return Ok(response);
    };
    require(
        sender.as_str() == PROPOSAL,
        "unexpected proposal hook sender",
    )?;
    require(
        env.block.height > boundary.height,
        "proposal execution blocked at the membership boundary height",
    )?;
    let stored = GATED.may_load(deps.storage)?.unwrap_or_default();
    let mut gated = Vec::with_capacity(stored.len());
    for &id in &stored {
        let proposal = load_proposal(deps.as_ref(), id)?;
        if stays_gated(deps.as_ref(), id, &proposal.status, &response.messages)? {
            gated.push(id);
        }
    }
    // Proposals created at the boundary height after the boundary was set are
    // only visible later. The common case is one query that finds none.
    let mut cursor = boundary.proposal_count;
    let mut limit = 1;
    'tail: loop {
        let page: ProposalList = deps.querier.query_wasm_smart(
            PROPOSAL,
            &ProposalQuery::ListProposals {
                start_after: Some(cursor),
                limit,
            },
        )?;
        let full = page.proposals.len() as u64 == limit;
        for record in page.proposals {
            require(record.id == cursor + 1, "proposal history gap")?;
            if record.proposal.start_height > boundary.height {
                break 'tail;
            }
            require(
                cursor - boundary.proposal_count < MAX_PROPOSALS_TO_CHECK,
                "too many proposals created at the boundary height",
            )?;
            cursor = record.id;
            if stays_gated(
                deps.as_ref(),
                record.id,
                &record.proposal.status,
                &response.messages,
            )? {
                gated.push(record.id);
            }
        }
        if !full {
            break;
        }
        limit = PROPOSALS_PER_QUERY;
    }
    require(
        gated.len() <= MAX_GATED,
        "too many unresolved proposals to block",
    )?;
    if gated != stored {
        GATED.save(deps.storage, &gated)?;
    }
    if cursor != boundary.proposal_count {
        BOUNDARY.save(
            deps.storage,
            &SnapshotBoundary {
                height: boundary.height,
                proposal_count: cursor,
            },
        )?;
    }
    guard_pinned_config(deps, response)
}

/// Admin calls can dispatch additional messages as the core during a proposal.
#[cfg(feature = "phoenix-rotation")]
pub(crate) fn guard_admin_messages(deps: Deps, response: Response) -> StdResult<Response> {
    if GUARDED_BATCH.may_load(deps.storage)?.is_some() {
        host::check_messages(&response)?;
    }
    Ok(response)
}

#[cfg(not(feature = "phoenix-rotation"))]
pub(crate) fn guard_admin_messages(_deps: Deps, response: Response) -> StdResult<Response> {
    Ok(response)
}

/// The outermost hook owns the reply and checks proposals created by nested
/// calls too. All dispatched messages and their replies share the transaction.
#[cfg(feature = "phoenix-rotation")]
fn guard_pinned_config(deps: DepsMut, mut response: Response) -> StdResult<Response> {
    if response.messages.is_empty() {
        return Ok(response);
    }
    host::check_messages(&response)?;
    let members = governance::current_members(deps.as_ref(), governance::MAX_REPAIR_MEMBERS)?;
    if let Some(batch) = GUARDED_BATCH.may_load(deps.storage)? {
        require(
            members == batch.members,
            "nested proposal execution blocked after membership changed",
        )?;
        return Ok(response);
    }
    let cfg: ProposalConfig = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Config {})?;
    let proposal_count = proposal_count(deps.as_ref())?;
    let seated_voting_period =
        governance::has_seat(deps.storage, &members)?.then_some(cfg.max_voting_period);
    GUARDED_BATCH.save(
        deps.storage,
        &GuardedBatch {
            proposal_count,
            veto: cfg.veto,
            members,
            seated_voting_period,
        },
    )?;
    let last = response
        .messages
        .last_mut()
        .expect("checked nonempty response");
    last.id = GUARD_REPLY_ID;
    last.reply_on = cosmwasm_std::ReplyOn::Success;
    Ok(response)
}

#[cfg(not(feature = "phoenix-rotation"))]
fn guard_pinned_config(_deps: DepsMut, response: Response) -> StdResult<Response> {
    Ok(response)
}

/// Handle the reply requested by the execution hook, if `id` is that reply.
#[cfg(feature = "phoenix-rotation")]
pub(crate) fn guard_reply(deps: DepsMut, env: &Env, id: u64) -> StdResult<Option<Response>> {
    if id != GUARD_REPLY_ID {
        return Ok(None);
    }
    let batch = GUARDED_BATCH.load(deps.storage)?;
    let cfg = rotation::check_membership_binding(deps.as_ref(), env)?;
    require(
        cfg.veto == batch.veto,
        "proposal veto configuration changed",
    )?;
    let members = governance::check_current_roster(deps.as_ref())?;
    require(
        governance::has_seat(deps.storage, &members)? == batch.seated_voting_period.is_some(),
        "changing governance seat membership requires governance",
    )?;
    if let Some(period) = batch.seated_voting_period {
        require(
            !cfg.allow_revoting && cfg.max_voting_period == period,
            "proposal voting settings changed with a governance seat",
        )?;
    }
    require(
        ADMIN.load(deps.storage)?.as_str() == CORE
            && NOMINATED_ADMIN.may_load(deps.storage)?.is_none(),
        "core admin or nomination changed",
    )?;
    let count = proposal_count(deps.as_ref())?;
    require(
        count
            .checked_sub(batch.proposal_count)
            .is_some_and(|n| n <= MAX_PROPOSALS_TO_CHECK),
        "too many proposals created during execution",
    )?;
    visit_proposals(deps.as_ref(), batch.proposal_count, count, |record| {
        require(
            record.proposal.threshold == cfg.threshold && record.proposal.veto == batch.veto,
            format!(
                "proposal {} saved different threshold or veto rules",
                record.id
            ),
        )?;
        if let Some(period) = batch.seated_voting_period {
            require(
                !record.proposal.allow_revoting
                    && record.proposal.expiration == period.after(&env.block),
                format!("proposal {} saved different voting settings", record.id),
            )?;
        }
        Ok(())
    })?;
    let mut response = Response::new();
    if members != batch.members {
        let boundary = BOUNDARY.load(deps.storage)?;
        let gated = plan_boundary(
            deps.as_ref(),
            boundary.proposal_count,
            boundary.proposal_count,
        )?
        .save(deps.storage, env.block.height)?;
        response = gated_attribute(
            response.add_attribute("membership_boundary", env.block.height.to_string()),
            &gated,
        );
    }
    GUARDED_BATCH.remove(deps.storage);
    Ok(Some(response))
}

#[cfg(not(feature = "phoenix-rotation"))]
pub(crate) fn guard_reply(_deps: DepsMut, _env: &Env, _id: u64) -> StdResult<Option<Response>> {
    Ok(None)
}

/// Adopts the unmigrated upstream 2.4.0 core, once. The default and governance
/// builds do not accept each other as a predecessor; see [`MigrateMsg`].
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
    let boundary = plan_boundary(deps.as_ref(), 0, expected_proposal_count)?;
    let mut changes = vec![MemberChange::Remove {
        address: outgoing.clone(),
    }];
    let mut transfer_admin = true;
    let mut allow_send = false;
    let mut seat = None;
    if let Some(opts) = governance {
        require(
            cfg!(feature = "phoenix-rotation")
                || (opts.seat.is_none() && !opts.seat_veto_only && !opts.allow_send_as_dao),
            "standing governance controls are not enabled in this build",
        )?;
        transfer_admin = opts.migration_admin == MigrationAdmin::GovernanceOnly;
        allow_send = opts.allow_send_as_dao;
        changes.extend(opts.membership_changes);
        require(
            !opts.seat_veto_only
                || opts
                    .seat
                    .as_ref()
                    .zip(opts.veto.as_ref())
                    .is_some_and(|(seat, veto)| seat.address == veto.vetoer),
            "a veto-only seat must be the configured vetoer",
        )?;
        if let Some(veto) = opts.veto {
            require(
                veto.vetoer == GOV || opts.seat.as_ref().is_some_and(|s| s.address == veto.vetoer),
                "expected governance vetoer",
            )?;
            cfg.veto = Some(veto);
        }
        if let Some(approved) = opts.seat {
            // A veto-only seat is recorded but holds no ballot. A later
            // membership action can seat it.
            if opts.seat_veto_only {
                governance::check_seat(deps.as_ref(), &approved)?;
            } else {
                changes.push(MemberChange::Add {
                    address: approved.address.clone(),
                    weight: SEAT_WEIGHT,
                });
            }
            seat = Some(approved);
        }
    }
    let previous = expected_members
        .iter()
        .cloned()
        .map(|addr| (addr, SEAT_WEIGHT))
        .collect();
    let membership = governance::plan_membership(deps.as_ref(), previous, changes, seat.as_ref())?;
    governance::configure_voting_seat(&mut cfg, membership.response_blocks)?;
    let gated = boundary.save(deps.storage, env.block.height)?;
    if let Some(seat) = seat {
        governance::SEAT.save(deps.storage, &seat)?;
    }
    let send = governance::set_send_enabled(deps.storage, env.block.time, allow_send)?;
    cfg.threshold = Threshold::AbsoluteCount {
        threshold: YES_WEIGHT,
    };
    let response = contract::migrate(deps, env, UpstreamMigrate::FromCompatible {})?;
    let response = gated_attribute(response, &gated)
        .add_message(WasmMsg::Execute {
            contract_addr: GROUP.to_string(),
            msg: to_json_binary(&membership.message)?,
            funds: vec![],
        })
        .add_message(WasmMsg::Execute {
            contract_addr: PROPOSAL.to_string(),
            msg: to_json_binary(&ProposalExecute::UpdateConfig(cfg))?,
            funds: vec![],
        })
        .add_attribute("action", "adopt_phoenix_4856")
        .add_attribute("removed", outgoing)
        .add_attribute("remaining_seats", membership.directors.to_string())
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
