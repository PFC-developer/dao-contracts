use cosmwasm_std::{from_json, Binary, CosmosMsg, Response, StdError, StdResult, WasmMsg};
use osmosis_std::types::{
    cosmos::authz::v1beta1::{GenericAuthorization, MsgExec, MsgGrant},
    cosmwasm::wasm::v1::{
        ContractExecutionAuthorization, ContractMigrationAuthorization, MsgClearAdmin,
        MsgExecuteContract, MsgMigrateContract, MsgUpdateAdmin,
    },
};

use super::{require, GroupExecute, CORE, GROUP, PROPOSAL, VOTING};
#[cfg(test)]
use super::{ProposalConfig, ProposalExecute};

fn companion(address: &str) -> bool {
    [GROUP, PROPOSAL, VOTING]
        .iter()
        .any(|protected| address.eq_ignore_ascii_case(protected))
}

fn change_admin_or_code(address: &str) -> StdResult<()> {
    require(
        !companion(address),
        "companion contract administration requires governance",
    )
}

fn grant_wasm_authority(address: &str) -> StdResult<()> {
    require(
        !companion(address) && !address.eq_ignore_ascii_case(CORE),
        "delegating protected contract authority requires governance",
    )
}

// The proposal module messages that install or remove code run on every
// status change, vote, or proposal. One of them can make a veto or the
// governance seat's deciding vote fail. Field contents are not inspected.
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposalModuleAdmin {
    UpdatePreProposeInfo {},
    AddProposalHook {},
    RemoveProposalHook {},
    AddVoteHook {},
    RemoveVoteHook {},
}

// Recognizes a config update by its name alone, whatever its fields.
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposalModuleConfig {
    UpdateConfig {},
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposalModuleDao {
    UpdateConfig { dao: String },
}

fn check_contract_execute(address: &str, msg: &[u8]) -> StdResult<()> {
    if address.eq_ignore_ascii_case(GROUP) {
        require(
            matches!(
                from_json::<GroupExecute>(msg)?,
                GroupExecute::UpdateMembers { .. }
            ),
            "changing group administration or hooks requires governance",
        )?;
    }
    if address.eq_ignore_ascii_case(PROPOSAL) {
        require(
            from_json::<ProposalModuleAdmin>(msg).is_err(),
            "changing proposal module hooks or pre-propose requires governance",
        )?;
        // A proposal can otherwise hand the module's DAO role to an
        // intermediary, have it perform one of the reserved admin calls, and
        // restore the DAO before the batch reply checks the final config.
        // Every config update must name the core, including one whose other
        // fields this crate cannot read.
        if from_json::<ProposalModuleConfig>(msg).is_ok() {
            require(
                matches!(
                    from_json::<ProposalModuleDao>(msg),
                    Ok(ProposalModuleDao::UpdateConfig { dao }) if dao.eq_ignore_ascii_case(CORE)
                ),
                "the proposal module's DAO must remain the core",
            )?;
        }
    }
    Ok(())
}

fn check_grant(value: Binary) -> StdResult<()> {
    let authorization = MsgGrant::try_from(value)?
        .grant
        .and_then(|grant| grant.authorization)
        .ok_or_else(|| StdError::generic_err("missing authorization"))?;
    let value = Binary::from(authorization.value);
    match authorization.type_url.as_str() {
        GenericAuthorization::TYPE_URL => require(
            !matches!(
                GenericAuthorization::try_from(value)?.msg.as_str(),
                MsgExec::TYPE_URL
                    | MsgGrant::TYPE_URL
                    | MsgExecuteContract::TYPE_URL
                    | MsgMigrateContract::TYPE_URL
                    | MsgUpdateAdmin::TYPE_URL
                    | MsgClearAdmin::TYPE_URL
            ),
            "delegating unrestricted contract authority requires governance",
        ),
        ContractExecutionAuthorization::TYPE_URL => {
            for grant in ContractExecutionAuthorization::try_from(value)?.grants {
                grant_wasm_authority(&grant.contract)?;
            }
            Ok(())
        }
        ContractMigrationAuthorization::TYPE_URL => {
            for grant in ContractMigrationAuthorization::try_from(value)?.grants {
                grant_wasm_authority(&grant.contract)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_native(type_url: &str, value: Binary, wrapped: bool) -> StdResult<()> {
    match type_url {
        MsgMigrateContract::TYPE_URL => {
            change_admin_or_code(&MsgMigrateContract::try_from(value)?.contract)
        }
        MsgClearAdmin::TYPE_URL => change_admin_or_code(&MsgClearAdmin::try_from(value)?.contract),
        MsgUpdateAdmin::TYPE_URL => {
            change_admin_or_code(&MsgUpdateAdmin::try_from(value)?.contract)
        }
        MsgExecuteContract::TYPE_URL => {
            let msg = MsgExecuteContract::try_from(value)?;
            check_contract_execute(&msg.contract, &msg.msg)
        }
        MsgGrant::TYPE_URL => check_grant(value),
        MsgExec::TYPE_URL => {
            require(
                !wrapped,
                "nested authorization execution requires governance",
            )?;
            for msg in MsgExec::try_from(value)?.msgs {
                check_native(&msg.type_url, Binary::from(msg.value), true)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The host accepts both typed Wasm messages and native protobuf messages.
/// Check both representations before the host can dispatch either one.
pub(super) fn check_messages(response: &Response) -> StdResult<()> {
    for submsg in &response.messages {
        match &submsg.msg {
            CosmosMsg::Wasm(
                WasmMsg::Migrate { contract_addr, .. }
                | WasmMsg::UpdateAdmin { contract_addr, .. }
                | WasmMsg::ClearAdmin { contract_addr },
            ) => change_admin_or_code(contract_addr)?,
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                check_contract_execute(contract_addr, msg.as_slice())?;
            }
            CosmosMsg::Stargate { type_url, value } => {
                check_native(type_url, value.clone(), false)?
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_messages_reserve_proposal_module_hooks_and_pre_propose() {
        for (msg, allowed) in [
            (r#"{"add_proposal_hook":{"address":"fixture-hook"}}"#, false),
            (
                r#"{"remove_proposal_hook":{"address":"fixture-hook"}}"#,
                false,
            ),
            (r#"{"add_vote_hook":{"address":"fixture-hook"}}"#, false),
            (r#"{"remove_vote_hook":{"address":"fixture-hook"}}"#, false),
            (
                r#"{"update_pre_propose_info":{"info":{"anyone_may_propose":{}}}}"#,
                false,
            ),
            (r#"{"execute":{"proposal_id":7}}"#, true),
        ] {
            let typed = WasmMsg::Execute {
                contract_addr: PROPOSAL.to_owned(),
                msg: Binary::from(msg.as_bytes()),
                funds: vec![],
            };
            let native = MsgExecuteContract {
                contract: PROPOSAL.to_owned(),
                msg: msg.as_bytes().to_vec(),
                ..Default::default()
            };
            assert_eq!(
                check_messages(&Response::new().add_message(typed)).is_ok(),
                allowed,
                "{msg}"
            );
            assert_eq!(
                check_messages(&Response::new().add_message(native)).is_ok(),
                allowed,
                "{msg}"
            );
            // The same messages sent to another contract are not this core's concern.
            let elsewhere = WasmMsg::Execute {
                contract_addr: "fixture-unrelated".to_owned(),
                msg: Binary::from(msg.as_bytes()),
                funds: vec![],
            };
            assert!(check_messages(&Response::new().add_message(elsewhere)).is_ok());
        }
    }

    // The single-choice module's `update_config` as it is sent on the wire,
    // written out so the check is tested against the module's format and not
    // against this crate's own types.
    fn update_config(dao: &str, optional: &str) -> String {
        format!(
            r#"{{"update_config":{{"threshold":{{"absolute_count":{{"threshold":"75"}}}},"max_voting_period":{{"time":604800}},{optional}"only_members_execute":false,"allow_revoting":false,"dao":"{dao}","close_proposal_on_execution_failure":false}}}}"#
        )
    }

    #[test]
    fn ordinary_messages_cannot_delegate_proposal_module_administration() {
        let other = "fixture-intermediary";
        let optional = r#""min_voting_period":null,"veto":null,"#;
        for (msg, allowed) in [
            (update_config(CORE, optional), true),
            (update_config(CORE, ""), true),
            (update_config(other, optional), false),
            (update_config(other, ""), false),
            // A config update this crate cannot fully read still has to name
            // the core.
            (format!(r#"{{"update_config":{{"dao":"{CORE}"}}}}"#), true),
            (
                r#"{"update_config":{"dao":"fixture-dao"}}"#.to_owned(),
                false,
            ),
            (
                r#"{"update_config":{"dao":"fixture-dao","later_field":1}}"#.to_owned(),
                false,
            ),
            (r#"{"update_config":{}}"#.to_owned(), false),
            (r#"{"update_config":{"dao":7}}"#.to_owned(), false),
            (
                format!(r#"{{"update_config":{{"dao":"{CORE}","dao":"fixture-dao"}}}}"#),
                false,
            ),
        ] {
            for contract in [PROPOSAL.to_owned(), PROPOSAL.to_ascii_uppercase()] {
                let typed = WasmMsg::Execute {
                    contract_addr: contract.clone(),
                    msg: Binary::from(msg.as_bytes()),
                    funds: vec![],
                };
                let native = MsgExecuteContract {
                    contract,
                    msg: msg.as_bytes().to_vec(),
                    ..Default::default()
                };
                let wrapped = MsgExec {
                    msgs: vec![native.to_any()],
                    ..Default::default()
                };
                assert_eq!(
                    check_messages(&Response::new().add_message(typed)).is_ok(),
                    allowed,
                    "{msg}"
                );
                assert_eq!(
                    check_messages(&Response::new().add_message(native)).is_ok(),
                    allowed,
                    "{msg}"
                );
                assert_eq!(
                    check_messages(&Response::new().add_message(wrapped)).is_ok(),
                    allowed,
                    "{msg}"
                );
            }
        }
    }

    // The core writes config updates with its own types. They must stay in
    // the wire format the check is tested against.
    #[test]
    fn core_config_updates_use_the_module_wire_format() {
        let written = cosmwasm_std::to_json_vec(&ProposalExecute::UpdateConfig(ProposalConfig {
            threshold: dao_voting::threshold::Threshold::AbsoluteCount {
                threshold: cosmwasm_std::Uint128::new(75),
            },
            max_voting_period: cw_utils::Duration::Time(604_800),
            min_voting_period: None,
            only_members_execute: false,
            allow_revoting: false,
            dao: CORE.to_owned(),
            close_proposal_on_execution_failure: false,
            veto: None,
        }))
        .unwrap();
        let written = String::from_utf8(written).unwrap();
        assert!(
            written.starts_with(r#"{"update_config":{"threshold":{"absolute_count":{"threshold":"75"}},"max_voting_period":{"time":604800},"min_voting_period":null,"only_members_execute":false,"allow_revoting":false,"dao":""#),
            "{written}"
        );
        assert!(
            written.ends_with(r#"","close_proposal_on_execution_failure":false,"veto":null}}"#),
            "{written}"
        );
        assert!(check_contract_execute(PROPOSAL, written.as_bytes()).is_ok());
    }

    #[test]
    fn ordinary_messages_allow_membership_updates_and_reserve_group_hooks() {
        use cosmwasm_std::to_json_binary;
        for (msg, allowed) in [
            (
                GroupExecute::UpdateMembers {
                    add: vec![],
                    remove: vec![],
                },
                true,
            ),
            (
                GroupExecute::AddHook {
                    addr: "fixture-hook".to_owned(),
                },
                false,
            ),
            (
                GroupExecute::RemoveHook {
                    addr: "fixture-hook".to_owned(),
                },
                false,
            ),
        ] {
            let msg = to_json_binary(&msg).unwrap();
            let typed = WasmMsg::Execute {
                contract_addr: GROUP.to_owned(),
                msg: msg.clone(),
                funds: vec![],
            };
            let native = MsgExecuteContract {
                contract: GROUP.to_owned(),
                msg: msg.to_vec(),
                ..Default::default()
            };
            assert_eq!(
                check_messages(&Response::new().add_message(typed)).is_ok(),
                allowed
            );
            assert_eq!(
                check_messages(&Response::new().add_message(native)).is_ok(),
                allowed
            );
        }
    }

    #[test]
    fn ordinary_dispatch_preserves_companion_administration() {
        for contract in [GROUP, PROPOSAL, VOTING] {
            let update = WasmMsg::UpdateAdmin {
                contract_addr: contract.to_owned(),
                admin: "fixture-admin".to_owned(),
            };
            assert!(check_messages(&Response::new().add_message(update)).is_err());
        }
        let allowed = WasmMsg::UpdateAdmin {
            contract_addr: "fixture-unrelated".to_owned(),
            admin: "fixture-admin".to_owned(),
        };
        assert!(check_messages(&Response::new().add_message(allowed)).is_ok());
    }

    #[test]
    fn native_admin_messages_use_the_same_companion_policy() {
        for (contract, allowed) in [(GROUP, false), ("fixture-unrelated", true)] {
            let update = MsgUpdateAdmin {
                sender: CORE.to_owned(),
                new_admin: "fixture-admin".to_owned(),
                contract: contract.to_owned(),
            };
            assert_eq!(
                check_messages(&Response::new().add_message(update)).is_ok(),
                allowed
            );
        }
    }

    #[test]
    fn host_guard_filters_native_authorization_by_permission() {
        for (type_url, allowed) in [
            (MsgExecuteContract::TYPE_URL, false),
            (MsgMigrateContract::TYPE_URL, false),
            (MsgUpdateAdmin::TYPE_URL, false),
            ("/cosmos.bank.v1beta1.MsgSend", true),
        ] {
            let grant = MsgGrant {
                grant: Some(osmosis_std::types::cosmos::authz::v1beta1::Grant {
                    authorization: Some(
                        GenericAuthorization {
                            msg: type_url.to_owned(),
                        }
                        .to_any(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(check_grant(grant.into()).is_ok(), allowed);
        }
    }
}
