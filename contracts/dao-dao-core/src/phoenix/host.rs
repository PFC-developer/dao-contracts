use cosmwasm_std::{Binary, CosmosMsg, Response, StdError, StdResult, WasmMsg};
use osmosis_std::types::{
    cosmos::authz::v1beta1::{GenericAuthorization, MsgExec, MsgGrant},
    cosmwasm::wasm::v1::{
        ContractExecutionAuthorization, ContractMigrationAuthorization, MsgClearAdmin,
        MsgExecuteContract, MsgMigrateContract, MsgUpdateAdmin,
    },
};

use super::{require, CORE, GROUP, PROPOSAL, VOTING};

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
