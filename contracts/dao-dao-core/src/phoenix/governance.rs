#[cfg(feature = "phoenix-rotation")]
use cosmwasm_std::{CosmosMsg, Empty};
use cosmwasm_std::{Storage, Timestamp};
use cw4::Member;
use std::collections::{BTreeMap, BTreeSet};

use super::*;

const SEND_DELAY_SECONDS: u64 = 48 * 60 * 60;
const SEND_ACTIVE_AT: Item<Timestamp> = Item::new("governance_send_active_at");
pub(super) const SEAT: Item<GovernanceSeat> = Item::new("governance_seat");

#[cw_serde]
pub enum MigrationAdmin {
    KeepCurrentAdmin,
    GovernanceOnly,
}

#[cw_serde]
pub struct GovernanceSeat {
    pub address: String,
    pub code_id: u64,
}

#[cw_serde]
pub struct GovernanceOptions {
    pub migration_admin: MigrationAdmin,
    /// Whether to schedule send-as-DAO activation at migration time.
    #[serde(default)]
    pub allow_send_as_dao: bool,
    /// An optional, reviewed governance voting proxy.
    pub seat: Option<GovernanceSeat>,
    /// Additional membership changes applied together with the required removal.
    #[serde(default)]
    pub membership_changes: Vec<MemberChange>,
    /// Veto configuration for newly created proposals.
    pub veto: Option<VetoConfig>,
}

#[cw_serde]
pub enum MemberChange {
    Add { address: String, weight: u64 },
    Remove { address: String },
    Reweight { address: String, weight: u64 },
}

#[derive(serde::Deserialize)]
struct SeatConfig {
    dao: String,
    group: String,
    proposal_module: String,
    seat_weight: u64,
}

#[cw_serde]
enum SeatQuery {
    Config {},
}

pub(super) fn check_seat(deps: Deps, seat: &GovernanceSeat) -> StdResult<()> {
    let address = deps.api.addr_validate(&seat.address)?;
    require(
        address.as_str() != CORE && address.as_str() != GOV,
        "expected separate governance seat",
    )?;
    let info = deps.querier.query_wasm_contract_info(&address)?;
    require(
        info.code_id == seat.code_id
            && (info.admin.is_none() || info.admin.as_deref() == Some(GOV)),
        "unexpected governance seat code or admin",
    )?;
    let cfg: SeatConfig = deps
        .querier
        .query_wasm_smart(&address, &SeatQuery::Config {})?;
    require(
        cfg.dao == CORE
            && cfg.group == GROUP
            && cfg.proposal_module == PROPOSAL
            && cfg.seat_weight == SEAT_WEIGHT,
        "unexpected governance seat binding",
    )
}

pub(super) fn check_context(env: &Env) -> StdResult<()> {
    require(
        env.block.chain_id == "phoenix-1" && env.contract.address.as_str() == CORE,
        "unexpected chain or core",
    )
}

pub(super) fn set_send_enabled(
    storage: &mut dyn Storage,
    now: Timestamp,
    enabled: bool,
) -> StdResult<Response> {
    if !enabled {
        SEND_ACTIVE_AT.remove(storage);
        return Ok(Response::new().add_attribute("action", "disable_governance_send"));
    }
    let active_at = match SEND_ACTIVE_AT.may_load(storage)? {
        Some(active_at) => active_at,
        None => {
            let active_at = now.plus_seconds(SEND_DELAY_SECONDS);
            SEND_ACTIVE_AT.save(storage, &active_at)?;
            active_at
        }
    };
    Ok(Response::new()
        .add_attribute("action", "schedule_governance_send")
        .add_attribute("active_at", active_at.seconds().to_string()))
}

#[cfg(feature = "phoenix-rotation")]
pub(super) fn send_as_dao(
    deps: Deps,
    now: Timestamp,
    msgs: Vec<CosmosMsg<Empty>>,
) -> StdResult<Response> {
    require(
        SEND_ACTIVE_AT
            .may_load(deps.storage)?
            .is_some_and(|active_at| now >= active_at),
        "governance send is disabled or pending",
    )?;
    require(!msgs.is_empty(), "expected at least one message")?;
    Ok(Response::new()
        .add_attribute("action", "governance_send")
        .add_messages(msgs))
}

#[cfg(feature = "phoenix-rotation")]
pub(super) fn reviewed_roster(
    deps: Deps,
    height: u64,
    expected: &[Member],
) -> StdResult<BTreeMap<String, u64>> {
    require(expected.len() <= 6, "too many reviewed members")?;
    let mut roster = BTreeMap::new();
    for m in expected {
        let addr = deps.api.addr_validate(&m.addr)?.into_string();
        require(
            m.weight > 0 && roster.insert(addr, m.weight).is_none(),
            "invalid reviewed members",
        )?;
    }
    let actual: MemberListResponse = deps.querier.query_wasm_smart(
        GROUP,
        &Cw4QueryMsg::ListMembers {
            start_after: None,
            limit: Some(7),
        },
    )?;
    require(
        actual.members.len() == roster.len()
            && actual
                .members
                .iter()
                .all(|m| roster.get(&m.addr) == Some(&m.weight)),
        "current roster or weights changed",
    )?;
    let total: TotalWeightResponse = deps.querier.query_wasm_smart(
        GROUP,
        &Cw4QueryMsg::TotalWeight {
            at_height: Some(height),
        },
    )?;
    require(
        u128::from(total.weight) == roster.values().map(|&w| u128::from(w)).sum(),
        "block-start total changed",
    )?;
    for (addr, &weight) in &roster {
        let member: MemberResponse = deps.querier.query_wasm_smart(
            GROUP,
            &Cw4QueryMsg::Member {
                addr: addr.clone(),
                at_height: Some(height),
            },
        )?;
        require(member.weight == Some(weight), "block-start member changed")?;
    }
    Ok(roster)
}

pub(super) fn plan_membership(
    deps: Deps,
    mut roster: BTreeMap<String, u64>,
    changes: Vec<MemberChange>,
    seat: Option<&GovernanceSeat>,
) -> StdResult<(GroupExecute, usize)> {
    require(
        (1..=12).contains(&changes.len()),
        "invalid membership change count",
    )?;
    let mut seen = BTreeSet::new();
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for change in changes {
        let addr = match &change {
            MemberChange::Add { address, .. }
            | MemberChange::Remove { address }
            | MemberChange::Reweight { address, .. } => {
                deps.api.addr_validate(address)?.into_string()
            }
        };
        require(seen.insert(addr.clone()), "duplicate membership operation")?;
        require(addr != CORE && addr != GOV, "reserved membership address")?;
        match change {
            MemberChange::Add { weight, .. } => {
                require(
                    weight > 0 && !roster.contains_key(&addr),
                    "invalid new member",
                )?;
                roster.insert(addr.clone(), weight);
                add.push(Member { addr, weight });
            }
            MemberChange::Reweight { weight, .. } => {
                require(
                    weight > 0 && roster.get(&addr).is_some_and(|&old| old != weight),
                    "invalid member reweight",
                )?;
                roster.insert(addr.clone(), weight);
                add.push(Member { addr, weight });
            }
            MemberChange::Remove { .. } => {
                require(
                    roster.remove(&addr).is_some(),
                    "member to remove is missing",
                )?;
                remove.push(addr);
            }
        }
    }
    let chain_seated = seat.is_some_and(|seat| roster.contains_key(&seat.address));
    let mut directors: Vec<_> = roster
        .iter()
        .filter(|(addr, _)| !seat.is_some_and(|seat| seat.address.as_str() == addr.as_str()))
        .map(|(_, &weight)| u128::from(weight))
        .collect();
    directors.sort_unstable_by(|a, b| b.cmp(a));
    require(
        matches!(directors.len(), 3..=5),
        "expected three to five directors",
    )?;
    require(
        directors[0] + directors[1] < YES_WEIGHT.u128()
            && directors.iter().sum::<u128>() >= YES_WEIGHT.u128()
            && roster.values().map(|&w| u128::from(w)).sum::<u128>() <= u128::from(u64::MAX),
        "weights must make the threshold reachable and require three directors",
    )?;
    if chain_seated {
        require(
            roster.values().all(|&w| w == SEAT_WEIGHT),
            "governance tie-breaking requires equal seat weights",
        )?;
    }
    Ok((GroupExecute::UpdateMembers { add, remove }, directors.len()))
}
