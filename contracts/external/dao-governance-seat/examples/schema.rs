use cosmwasm_schema::write_api;
use dao_governance_seat::{InstantiateMsg, QueryMsg, SudoMsg};

fn main() {
    write_api! {
        instantiate: InstantiateMsg,
        query: QueryMsg,
        sudo: SudoMsg,
    }
}
