use cosmwasm_schema::cw_serde;
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, to_json_vec, Addr, CosmosMsg, Deps, DepsMut, Empty, Env, HexBinary, Order,
    Response, StdError, StdResult, Storage, SubMsg, Uint128, WasmMsg,
};
use cw2::get_contract_version;
use cw4::{
    AdminResponse, Cw4QueryMsg, HooksResponse, MemberListResponse, MemberResponse,
    TotalWeightResponse,
};
use cw4_group::msg::ExecuteMsg as GroupExecute;
use cw_storage_plus::{Bound, Item, Map};
use cw_utils::Duration;
use dao_interface::{msg::MigrateMsg as UpstreamMigrate, state::ProposalModuleStatus, voting};
use dao_voting::{
    pre_propose::ProposalCreationPolicy, status::Status, threshold::Threshold, veto::VetoConfig,
};
use sha2::{Digest, Sha256};

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

// Bounds the work one execution does for blocked proposals that carry the same
// messages as the executing one. It never limits a boundary. An execution
// reads at most twice this many rows, drops the settled ones, and fails only
// if more than this many still block.
const MAX_GATED_PER_PAYLOAD: usize = 64;
const MAX_PAYLOAD_ROWS_READ: usize = 2 * MAX_GATED_PER_PAYLOAD;
// Settled rows each successful execution retires, whatever its messages.
const ROWS_PRUNED_PER_EXECUTION: usize = 4;
#[cfg(feature = "phoenix-rotation")]
const GUARD_REPLY_ID: u64 = 4856;
#[cfg(feature = "phoenix-rotation")]
const SEND_REPLY_ID: u64 = 4857;

const BOUNDARY: Item<SnapshotBoundary> = Item::new("phoenix_snapshot_boundary_v1");
/// Proposals that were unresolved when a boundary was set. They snapshot an
/// earlier roster, so the core refuses to dispatch their messages.
///
/// The execution hook carries messages but no proposal ID, so blocked IDs are
/// filed under a key derived from their messages. An execution reads only the
/// entry for its own messages, however many proposals are blocked in total.
/// Keys come from this build's encoding of the messages, so a successor build
/// must keep that encoding or refile the entries.
type PayloadKey = [u8; 32];
const GATED: Map<(&[u8], u64), ()> = Map::new("phoenix_gated_messages_v2");
/// Rows beyond the boundary are staged by checkpointing and do not gate execution.
const INDEXED: Map<u64, Option<PayloadKey>> = Map::new("phoenix_indexed_proposals_v2");
const PRUNE_CURSOR: Item<u64> = Item::new("phoenix_proposal_prune_cursor_v2");
#[cfg(feature = "phoenix-rotation")]
const PENDING_HISTORY: Item<PendingHistory> = Item::new("phoenix_pending_history_v2");
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
        /// Complete, reviewed list of hooks registered on the membership group.
        /// A hook runs on every membership change and can make it fail.
        #[serde(default)]
        expected_group_hooks: Vec<String>,
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

#[derive(serde::Deserialize)]
struct ExecutedRecord {
    id: u64,
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
}

#[cfg(feature = "phoenix-rotation")]
#[cw_serde]
struct GuardedBatch {
    proposal_count: u64,
    veto: Option<VetoConfig>,
    members: Vec<cw4::Member>,
    hooks: Vec<String>,
    seated_voting_period: Option<Duration>,
}

struct PlannedBoundary {
    proposal_count: u64,
    /// Newly blocked proposals with the key their messages are filed under.
    /// A proposal whose messages can never reach the core has no key.
    gated: Vec<(u64, Option<PayloadKey>)>,
    /// Still-unresolved proposals among the first staged rows the boundary
    /// activates, and whether more staged rows exist than were read.
    already_indexed: Vec<u64>,
    more_staged: bool,
    settled: Vec<(u64, Option<PayloadKey>)>,
}

/// The proposals a boundary reports as newly blocked.
struct Blocked {
    ids: Vec<u64>,
    /// More staged proposals were activated than the report lists.
    truncated: bool,
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

fn group_hooks(deps: Deps) -> StdResult<Vec<String>> {
    let response: HooksResponse = deps
        .querier
        .query_wasm_smart(GROUP, &Cw4QueryMsg::Hooks {})?;
    let mut hooks = response.hooks;
    hooks.sort_unstable();
    Ok(hooks)
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

// Statuses from which a proposal can never execute. `Executed` is not one of
// them: during an execution hook it is also the status of the running proposal.
fn is_dead(status: &Status) -> bool {
    matches!(
        status,
        Status::Rejected | Status::Closed | Status::ExecutionFailed | Status::Vetoed
    )
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

#[cfg(feature = "phoenix-rotation")]
fn checkpoint_history(mut deps: DepsMut, boundary: &SnapshotBoundary) -> StdResult<Response> {
    // Checkpoints run as native governance, outside a proposal hook, so an
    // executed proposal is settled here and its row can go too.
    let (pruned, prune_complete) = prune_rows(
        deps.branch(),
        PROPOSALS_PER_CHECKPOINT as usize,
        is_resolved,
    )?;
    let through = PENDING_HISTORY
        .may_load(deps.storage)?
        .map_or(boundary.proposal_count, |history| {
            history.through.max(boundary.proposal_count)
        });
    let count = proposal_count(deps.as_ref())?;
    require(count >= through, "proposal count moved backwards")?;
    let end = count.min(through.saturating_add(PROPOSALS_PER_CHECKPOINT));
    let mut tail = boundary.proposal_count;
    let mut staged = Vec::new();
    visit_proposals(deps.as_ref(), through, end, |record| {
        if record.proposal.start_height <= boundary.height {
            tail = record.id;
        }
        if !is_resolved(&record.proposal.status) {
            staged.push((record.id, gate_key(deps.as_ref(), record.id)?));
        }
        Ok(())
    })?;
    for (id, key) in staged {
        file_gated(deps.storage, id, key)?;
    }
    if tail > boundary.proposal_count {
        BOUNDARY.save(
            deps.storage,
            &SnapshotBoundary {
                height: boundary.height,
                proposal_count: tail,
            },
        )?;
    }
    PENDING_HISTORY.save(deps.storage, &PendingHistory { through: end })?;
    Ok(Response::new()
        .add_attribute("action", "checkpoint_proposal_history")
        .add_attribute("checked_through", end.to_string())
        .add_attribute("proposal_count", count.to_string())
        .add_attribute("boundary_cursor", tail.to_string())
        .add_attribute("pruned", pruned.to_string())
        .add_attribute("prune_complete", prune_complete.to_string()))
}

fn payload_key<'a>(msgs: impl IntoIterator<Item = &'a CosmosMsg<Empty>>) -> StdResult<PayloadKey> {
    let messages: Vec<_> = msgs.into_iter().collect();
    Ok(Sha256::digest(to_json_vec(&messages)?).into())
}

fn load_messages(deps: Deps, id: u64) -> StdResult<Vec<CosmosMsg<Empty>>> {
    let record: ExecutedRecord = deps
        .querier
        .query_wasm_smart(PROPOSAL, &ProposalQuery::Proposal { proposal_id: id })?;
    require(record.id == id, "unexpected proposal record")?;
    Ok(record.proposal.msgs)
}

// Only a verified empty batch can be omitted from the message index.
fn gate_key(deps: Deps, id: u64) -> StdResult<Option<PayloadKey>> {
    let msgs = load_messages(deps, id)?;
    if msgs.is_empty() {
        return Ok(None);
    }
    payload_key(&msgs).map(Some)
}

fn file_gated(storage: &mut dyn Storage, id: u64, key: Option<PayloadKey>) -> StdResult<()> {
    if let Some(key) = key {
        GATED.save(storage, (&key, id), &())?;
    }
    INDEXED.save(storage, id, &key)
}

fn remove_indexed(storage: &mut dyn Storage, id: u64, key: Option<PayloadKey>) {
    if let Some(key) = key {
        GATED.remove(storage, (&key, id));
    }
    INDEXED.remove(storage, id);
}

// Walk the index a bounded batch at a time and drop the rows `settled`
// accepts. The cursor wraps, so repeated calls cover every row. Returns the
// number of rows dropped and whether this call reached the end of the index.
fn prune_rows(
    deps: DepsMut,
    batch: usize,
    settled: fn(&Status) -> bool,
) -> StdResult<(usize, bool)> {
    let start = PRUNE_CURSOR.may_load(deps.storage)?.map(Bound::exclusive);
    let entries = INDEXED
        .range(deps.storage, start, None, Order::Ascending)
        .take(batch)
        .collect::<StdResult<Vec<_>>>()?;
    let mut pruned = 0;
    for &(id, key) in &entries {
        if settled(&load_proposal(deps.as_ref(), id)?.status) {
            remove_indexed(deps.storage, id, key);
            pruned += 1;
        }
    }
    let complete = entries.len() < batch;
    if complete {
        PRUNE_CURSOR.remove(deps.storage);
    } else {
        PRUNE_CURSOR.save(deps.storage, &entries.last().expect("full batch").0)?;
    }
    Ok((pruned, complete))
}

// Classify proposals since the last boundary. Those up to the reviewed ID
// must be resolved. Any later unresolved proposals are gated instead, however
// many there are, so opening proposals cannot stop a boundary from being set.
fn plan_boundary(deps: Deps, checked: u64, reviewed: u64) -> StdResult<PlannedBoundary> {
    let count = proposal_count(deps)?;
    require(
        count >= reviewed,
        "proposal count is below the reviewed count",
    )?;
    #[cfg(feature = "phoenix-rotation")]
    let through = PENDING_HISTORY
        .may_load(deps.storage)?
        .map_or(checked, |history| history.through.max(checked));
    #[cfg(not(feature = "phoenix-rotation"))]
    let through = checked;
    require(
        count
            .checked_sub(through)
            .is_some_and(|n| n <= MAX_PROPOSALS_TO_CHECK),
        "proposal count exceeds the scan window; checkpoint proposal history first",
    )?;
    let mut gated = Vec::new();
    let mut gate = |id| -> StdResult<()> {
        require(
            id > reviewed,
            format!("resolve legacy proposal {id} before changing membership"),
        )?;
        gated.push((id, gate_key(deps, id)?));
        Ok(())
    };
    let mut settled = Vec::new();
    let staged_review_end = through.min(reviewed);
    if staged_review_end > checked {
        for (offset, entry) in INDEXED
            .range(
                deps.storage,
                Some(Bound::exclusive(checked)),
                Some(Bound::inclusive(staged_review_end)),
                Order::Ascending,
            )
            .take(PROPOSALS_PER_QUERY as usize + 1)
            .enumerate()
        {
            require(
                offset < PROPOSALS_PER_QUERY as usize,
                "checkpoint proposal history to clear settled reviewed entries",
            )?;
            let (id, key) = entry?;
            if !is_resolved(&load_proposal(deps, id)?.status) {
                gate(id)?;
            }
            settled.push((id, key));
        }
    }
    visit_proposals(deps, through, count, |record| {
        if !is_resolved(&record.proposal.status) {
            gate(record.id)?;
        }
        Ok(())
    })?;
    // Staged entries already have message keys. Committing the new proposal
    // count activates every staged row. Report the ones among a bounded first
    // batch that can still execute, and say when there are more.
    let mut already_indexed = Vec::new();
    let mut more_staged = false;
    if through > checked.max(reviewed) {
        for (offset, id) in INDEXED
            .keys(
                deps.storage,
                Some(Bound::exclusive(checked.max(reviewed))),
                Some(Bound::inclusive(through)),
                Order::Ascending,
            )
            .take(PROPOSALS_PER_QUERY as usize + 1)
            .enumerate()
        {
            let id = id?;
            if offset == PROPOSALS_PER_QUERY as usize {
                more_staged = true;
            } else if !is_resolved(&load_proposal(deps, id)?.status) {
                already_indexed.push(id);
            }
        }
    }
    Ok(PlannedBoundary {
        proposal_count: count,
        gated,
        already_indexed,
        more_staged,
        settled,
    })
}

impl PlannedBoundary {
    /// Record the boundary and return the proposals it newly blocks.
    fn save(self, storage: &mut dyn Storage, height: u64) -> StdResult<Blocked> {
        #[cfg(feature = "phoenix-rotation")]
        PENDING_HISTORY.remove(storage);
        for (id, key) in self.settled {
            remove_indexed(storage, id, key);
        }
        for &(id, key) in &self.gated {
            file_gated(storage, id, key)?;
        }
        BOUNDARY.save(
            storage,
            &SnapshotBoundary {
                height,
                proposal_count: self.proposal_count,
            },
        )?;
        Ok(Blocked {
            ids: self
                .already_indexed
                .into_iter()
                .chain(self.gated.into_iter().map(|(id, _)| id))
                .collect(),
            truncated: self.more_staged,
        })
    }
}

fn gated_attribute(mut response: Response, blocked: &Blocked) -> Response {
    if !blocked.ids.is_empty() {
        let ids: Vec<_> = blocked.ids.iter().map(u64::to_string).collect();
        response = response.add_attribute("gated_proposals", ids.join(","));
    }
    if blocked.truncated {
        response = response.add_attribute("gated_proposals_truncated", "true");
    }
    response
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
            let msgs = load_messages(deps, id)?;
            let running = msgs.iter().eq(hook.iter().map(|m| &m.msg));
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
    mut deps: DepsMut,
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
            // Governance can advance a long tail in bounded checkpoints. The
            // default build has no other path, so there this execution must
            // finish the tail itself: filing a proposal costs less than
            // creating one, and every proposal in the tail was created in a
            // single block.
            #[cfg(feature = "phoenix-rotation")]
            require(
                cursor - boundary.proposal_count < MAX_PROPOSALS_TO_CHECK,
                "too many proposals created at the boundary height; \
                 checkpoint proposal history first",
            )?;
            cursor = record.id;
            if stays_gated(
                deps.as_ref(),
                record.id,
                &record.proposal.status,
                &response.messages,
            )? {
                let key = gate_key(deps.as_ref(), record.id)?;
                file_gated(deps.storage, record.id, key)?;
            }
        }
        if !full {
            break;
        }
        limit = PROPOSALS_PER_QUERY;
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
    // Only blocked proposals that carry these messages can be the one running.
    // Settled rows are dropped before the rest are counted, so a payload is
    // usable again once enough of its blocked proposals have settled.
    let key = payload_key(response.messages.iter().map(|m| &m.msg))?;
    let candidates = GATED
        .prefix(&key)
        .keys(
            deps.storage,
            None,
            Some(Bound::inclusive(cursor)),
            Order::Ascending,
        )
        .take(MAX_PAYLOAD_ROWS_READ + 1)
        .collect::<StdResult<Vec<_>>>()?;
    require(
        candidates.len() <= MAX_PAYLOAD_ROWS_READ,
        "too many blocked proposals carry these messages",
    )?;
    let mut blocking = 0;
    for id in candidates {
        let proposal = load_proposal(deps.as_ref(), id)?;
        if stays_gated(deps.as_ref(), id, &proposal.status, &response.messages)? {
            blocking += 1;
        } else {
            remove_indexed(deps.storage, id, Some(key));
        }
    }
    require(
        blocking <= MAX_GATED_PER_PAYLOAD,
        "too many blocked proposals carry these messages",
    )?;
    #[cfg(feature = "phoenix-rotation")]
    if PENDING_HISTORY.may_load(deps.storage)?.is_some() {
        // Staged proposals can still execute. The running one may be among
        // them, so every staged row for these messages is read: a row left
        // behind for an executed proposal would be activated by the next
        // boundary and then refuse each later proposal with the same messages.
        let staged = GATED
            .prefix(&key)
            .keys(
                deps.storage,
                Some(Bound::exclusive(cursor)),
                None,
                Order::Ascending,
            )
            .take(MAX_GATED_PER_PAYLOAD + 1)
            .collect::<StdResult<Vec<_>>>()?;
        require(
            staged.len() <= MAX_GATED_PER_PAYLOAD,
            "too many staged proposals carry these messages",
        )?;
        for id in staged {
            if is_resolved(&load_proposal(deps.as_ref(), id)?.status) {
                remove_indexed(deps.storage, id, Some(key));
            }
        }
    }
    // Retire a few rows of proposals that can never execute, whatever their
    // messages. This is what clears a payload with more rows than one
    // execution reads, including in the default build, which has no
    // checkpoint. It never drops an executed row, which could be a blocked
    // proposal that is running in this transaction.
    prune_rows(deps.branch(), ROWS_PRUNED_PER_EXECUTION, is_dead)?;
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
///
/// If the batch migrates this core, the reply is delivered to the successor
/// code. A successor must handle reply `GUARD_REPLY_ID` (and `SEND_REPLY_ID`
/// for a governance send) or the batch fails; a self-administered core moves
/// to other code by reassigning its host admin first.
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
    let hooks = group_hooks(deps.as_ref())?;
    GUARDED_BATCH.save(
        deps.storage,
        &GuardedBatch {
            proposal_count,
            veto: cfg.veto,
            members,
            hooks,
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
    if id == SEND_REPLY_ID {
        return governance::send_reply(deps, env).map(Some);
    }
    if id != GUARD_REPLY_ID {
        return Ok(None);
    }
    let batch = GUARDED_BATCH.load(deps.storage)?;
    let cfg = rotation::check_membership_binding(deps.as_ref(), env)?;
    require(
        cfg.veto == batch.veto,
        "proposal veto configuration changed",
    )?;
    // A hook runs inside every membership update and can make it fail.
    require(
        group_hooks(deps.as_ref())? == batch.hooks,
        "changing group hooks requires governance",
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
        expected_group_hooks,
        governance,
    } = msg;
    let mut expected_group_hooks = expected_group_hooks
        .iter()
        .map(|hook| deps.api.addr_validate(hook).map(Addr::into_string))
        .collect::<StdResult<Vec<_>>>()?;
    expected_group_hooks.sort_unstable();
    require(
        group_hooks(deps.as_ref())? == expected_group_hooks,
        "group hooks differ from the reviewed list",
    )?;
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
