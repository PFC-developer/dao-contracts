use cosmwasm_schema::write_api;
use dao_dao_core::phoenix::MigrateMsg;
#[cfg(feature = "phoenix-rotation")]
use dao_dao_core::phoenix::SudoMsg;
use dao_interface::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};

fn main() {
    #[cfg(not(feature = "phoenix-rotation"))]
    write_api! {
        instantiate: InstantiateMsg,
        query: QueryMsg,
        execute: ExecuteMsg,
        migrate: MigrateMsg,
    }

    #[cfg(feature = "phoenix-rotation")]
    write_api! {
        instantiate: InstantiateMsg,
        query: QueryMsg,
        execute: ExecuteMsg,
        migrate: MigrateMsg,
        sudo: SudoMsg,
    }
}
