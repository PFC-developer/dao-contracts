use cosmwasm_schema::{cw_serde, QueryResponses};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Addr, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult,
    Uint128, WasmMsg,
};
use cw4::{Cw4QueryMsg, MemberListResponse, MemberResponse};
use cw_storage_plus::Item;
use dao_voting::{
    status::Status,
    threshold::Threshold,
    voting::{Vote, Votes},
};

const CONFIG: Item<Config> = Item::new("config");

#[cw_serde]
pub struct InstantiateMsg {
    pub dao: String,
    pub group: String,
    pub proposal_module: String,
    pub seat_weight: u64,
    /// Blocks a proposal must remain open before uncast votes qualify as deadlock.
    pub response_blocks: u64,
}

#[cw_serde]
pub struct Config {
    pub dao: Addr,
    pub group: Addr,
    pub proposal_module: Addr,
    pub seat_weight: u64,
    pub response_blocks: u64,
    voting_module: Addr,
    group_code_id: u64,
    proposal_code_id: u64,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(Config)]
    Config {},
}

#[cw_serde]
pub enum SudoMsg {
    /// Cast the deciding YES on an open, currently deadlocked proposal.
    BreakTie {
        proposal_id: u64,
        expected_start_height: u64,
    },
    /// Request the proposal module's ordinary veto.
    Veto {
        proposal_id: u64,
        expected_start_height: u64,
    },
}

#[cw_serde]
enum VotingQuery {
    GroupContract {},
}

#[cw_serde]
enum ProposalQuery {
    Dao {},
    Proposal { proposal_id: u64 },
    GetVote { proposal_id: u64, voter: String },
}

#[derive(serde::Deserialize)]
struct ProposalResponse {
    id: u64,
    proposal: Proposal,
}

#[derive(serde::Deserialize)]
struct Proposal {
    start_height: u64,
    status: Status,
    threshold: Threshold,
    total_power: Uint128,
    votes: Votes,
    allow_revoting: bool,
    delegation_module: Option<Addr>,
}

#[derive(serde::Deserialize)]
struct VoteResponse {
    vote: Option<serde::de::IgnoredAny>,
}

#[cw_serde]
enum ProposalExecute {
    Vote {
        proposal_id: u64,
        vote: Vote,
        rationale: Option<String>,
    },
    Veto {
        proposal_id: u64,
    },
}

fn require(ok: bool, error: &'static str) -> StdResult<()> {
    if ok {
        Ok(())
    } else {
        Err(StdError::generic_err(error))
    }
}

fn voting_module(deps: Deps, dao: &Addr) -> StdResult<Addr> {
    deps.querier
        .query_wasm_smart(dao, &dao_interface::msg::QueryMsg::VotingModule {})
}

fn validate_bindings(deps: Deps, config: &Config) -> StdResult<()> {
    let voting = voting_module(deps, &config.dao)?;
    let group: Addr = deps
        .querier
        .query_wasm_smart(&voting, &VotingQuery::GroupContract {})?;
    let dao: Addr = deps
        .querier
        .query_wasm_smart(&config.proposal_module, &ProposalQuery::Dao {})?;
    require(
        dao == config.dao
            && group == config.group
            && voting == config.voting_module
            && deps.querier.query_wasm_contract_info(&group)?.code_id == config.group_code_id
            && deps
                .querier
                .query_wasm_contract_info(&config.proposal_module)?
                .code_id
                == config.proposal_code_id,
        "governance seat bindings changed",
    )
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    require(
        info.funds.is_empty(),
        "seat initialization does not accept funds",
    )?;
    require(
        msg.seat_weight > 0 && msg.response_blocks > 0,
        "expected nonzero weight and response window",
    )?;
    let dao = deps.api.addr_validate(&msg.dao)?;
    let group = deps.api.addr_validate(&msg.group)?;
    let proposal_module = deps.api.addr_validate(&msg.proposal_module)?;
    require(
        ![&dao, &group, &proposal_module].contains(&&env.contract.address),
        "governance seat must be a separate address",
    )?;
    let config = Config {
        voting_module: voting_module(deps.as_ref(), &dao)?,
        group_code_id: deps.querier.query_wasm_contract_info(&group)?.code_id,
        proposal_code_id: deps
            .querier
            .query_wasm_contract_info(&proposal_module)?
            .code_id,
        dao,
        group,
        proposal_module,
        seat_weight: msg.seat_weight,
        response_blocks: msg.response_blocks,
    };
    validate_bindings(deps.as_ref(), &config)?;
    CONFIG.save(deps.storage, &config)?;
    cw2::set_contract_version(
        deps.storage,
        "crates.io:dao-governance-seat",
        env!("CARGO_PKG_VERSION"),
    )?;
    Ok(Response::new())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&CONFIG.load(deps.storage)?),
    }
}

// There is no ordinary execute entry point. The host authorizes sudo, and the
// proposal module records the separate seat contract as the ballot sender.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn sudo(deps: DepsMut, env: Env, msg: SudoMsg) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    validate_bindings(deps.as_ref(), &config)?;
    let (proposal_id, expected_start_height) = match msg {
        SudoMsg::BreakTie {
            proposal_id,
            expected_start_height,
        }
        | SudoMsg::Veto {
            proposal_id,
            expected_start_height,
        } => (proposal_id, expected_start_height),
    };
    let proposal: ProposalResponse = deps.querier.query_wasm_smart(
        &config.proposal_module,
        &ProposalQuery::Proposal { proposal_id },
    )?;
    require(
        proposal.id == proposal_id && proposal.proposal.start_height == expected_start_height,
        "proposal snapshot changed",
    )?;
    let (action, message) = match msg {
        SudoMsg::BreakTie { .. } => {
            let props = &proposal.proposal;
            let directors = verify_snapshot(deps.as_ref(), &env, &config, props)?;
            let prior: VoteResponse = deps.querier.query_wasm_smart(
                &config.proposal_module,
                &ProposalQuery::GetVote {
                    proposal_id,
                    voter: env.contract.address.to_string(),
                },
            )?;
            require(prior.vote.is_none(), "governance seat has already voted")?;
            check_deadlock(props, directors, &config, env.block.height)?;
            (
                "break_tie",
                ProposalExecute::Vote {
                    proposal_id,
                    vote: Vote::Yes,
                    rationale: None,
                },
            )
        }
        SudoMsg::Veto { .. } => ("veto", ProposalExecute::Veto { proposal_id }),
    };
    Ok(Response::new()
        .add_message(WasmMsg::Execute {
            contract_addr: config.proposal_module.into_string(),
            msg: to_json_binary(&message)?,
            funds: vec![],
        })
        .add_attribute("action", action)
        .add_attribute("proposal_id", proposal_id.to_string()))
}

fn verify_snapshot(deps: Deps, env: &Env, config: &Config, proposal: &Proposal) -> StdResult<u64> {
    let group: MemberListResponse = deps.querier.query_wasm_smart(
        &config.group,
        &Cw4QueryMsg::ListMembers {
            start_after: None,
            limit: Some(6),
        },
    )?;
    let count = group.members.len() as u64;
    require(
        (4..=5).contains(&count)
            && proposal.total_power == Uint128::from(config.seat_weight) * Uint128::from(count)
            && group.members.iter().any(|m| m.addr == env.contract.address),
        "expected three or four directors and a seated governance voter",
    )?;
    for member in group.members {
        let historical: MemberResponse = deps.querier.query_wasm_smart(
            &config.group,
            &Cw4QueryMsg::Member {
                addr: member.addr,
                at_height: Some(proposal.start_height),
            },
        )?;
        require(
            member.weight == config.seat_weight && historical.weight == Some(config.seat_weight),
            "proposal seat snapshot no longer matches",
        )?;
    }
    Ok(count - 1)
}

fn check_deadlock(
    proposal: &Proposal,
    directors: u64,
    config: &Config,
    height: u64,
) -> StdResult<()> {
    let weight = Uint128::from(config.seat_weight);
    require(
        proposal.status == Status::Open
            && !proposal.allow_revoting
            && proposal.delegation_module.is_none()
            && proposal.threshold
                == (Threshold::AbsoluteCount {
                    threshold: weight * Uint128::new(3),
                }),
        "proposal is not open under the supported voting rules",
    )?;
    // The seat has not voted, so every recorded ballot belongs to a director.
    // A completed 2–1 or 2–2 split qualifies immediately. When only the two
    // YES ballots have arrived, the other directors get the full window.
    let split = proposal.votes.no == weight.checked_mul(Uint128::from(directors - 2))?;
    let elapsed = height >= proposal.start_height.saturating_add(config.response_blocks);
    require(
        proposal.votes.yes == weight.checked_mul(Uint128::new(2))?
            && proposal.votes.abstain.is_zero()
            && (split || (elapsed && proposal.votes.no.is_zero())),
        "proposal is not deadlocked",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(yes: u128, no: u128, abstain: u128) -> (Proposal, Config) {
        let config = Config {
            dao: Addr::unchecked("fixture-dao"),
            group: Addr::unchecked("fixture-group"),
            proposal_module: Addr::unchecked("fixture-proposal"),
            voting_module: Addr::unchecked("fixture-voting"),
            seat_weight: 25,
            response_blocks: 10,
            group_code_id: 1,
            proposal_code_id: 2,
        };
        let proposal = Proposal {
            start_height: 100,
            status: Status::Open,
            threshold: Threshold::AbsoluteCount {
                threshold: Uint128::new(75),
            },
            total_power: Uint128::new(125),
            votes: Votes {
                yes: yes.into(),
                no: no.into(),
                abstain: abstain.into(),
            },
            allow_revoting: false,
            delegation_module: None,
        };
        (proposal, config)
    }

    #[test]
    fn accepts_complete_splits_with_three_or_four_directors() {
        for (directors, no) in [(3, 25), (4, 50)] {
            let (proposal, config) = setup(50, no, 0);
            assert!(check_deadlock(&proposal, directors, &config, 101).is_ok());
        }
    }

    #[test]
    fn silence_requires_the_full_response_window() {
        let (proposal, config) = setup(50, 0, 0);
        for directors in [3, 4] {
            assert!(check_deadlock(&proposal, directors, &config, 109).is_err());
            assert!(check_deadlock(&proposal, directors, &config, 110).is_ok());
        }
    }

    #[test]
    fn completed_ballots_with_abstentions_do_not_qualify() {
        let (proposal, config) = setup(50, 0, 25);
        assert!(check_deadlock(&proposal, 3, &config, 110).is_err());
        let (proposal, config) = setup(50, 25, 25);
        assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
        let (proposal, config) = setup(50, 0, 50);
        assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
    }

    #[test]
    fn partial_ballots_with_a_no_or_abstention_do_not_qualify() {
        for (no, abstain) in [(25, 0), (0, 25)] {
            let (proposal, config) = setup(50, no, abstain);
            assert!(check_deadlock(&proposal, 4, &config, 101).is_err());
            assert!(check_deadlock(&proposal, 4, &config, 109).is_err());
            assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
        }
    }

    #[test]
    fn requires_exactly_two_director_yes_votes() {
        for (yes, no, abstain) in [
            (0, 0, 0),
            (25, 0, 0),
            (25, 50, 0),
            (25, 75, 0),
            (25, 25, 50),
            (75, 0, 0),
            (75, 25, 0),
            (100, 0, 0),
        ] {
            let (proposal, config) = setup(yes, no, abstain);
            for directors in [3, 4] {
                for height in [101, 110, 1_000] {
                    assert!(check_deadlock(&proposal, directors, &config, height).is_err());
                }
            }
        }
    }

    #[test]
    fn refuses_closed_proposals_and_changed_rules() {
        let (mut proposal, config) = setup(50, 50, 0);
        proposal.status = Status::Passed;
        assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
        proposal.status = Status::Open;
        proposal.allow_revoting = true;
        assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
        proposal.allow_revoting = false;
        proposal.delegation_module = Some(Addr::unchecked("fixture-delegation"));
        assert!(check_deadlock(&proposal, 4, &config, 110).is_err());
    }
}
