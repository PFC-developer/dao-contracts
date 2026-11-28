#[cfg(not(feature = "phoenix-rotation"))]
use cosmwasm_schema::write_api;
use dao_dao_core::phoenix::MigrateMsg;
#[cfg(feature = "phoenix-rotation")]
use dao_dao_core::phoenix::SudoMsg;
use dao_interface::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};

// `scripts/schema.sh` runs this example once per build. The default build
// writes schema/dao-dao-core.json. The rotation build adds a sudo entry point
// and writes schema/phoenix-rotation/dao-dao-core.json, so both stay committed.
fn main() {
    #[cfg(not(feature = "phoenix-rotation"))]
    write_api! {
        instantiate: InstantiateMsg,
        query: QueryMsg,
        execute: ExecuteMsg,
        migrate: MigrateMsg,
    }

    #[cfg(feature = "phoenix-rotation")]
    {
        let api = cosmwasm_schema::generate_api! {
            instantiate: InstantiateMsg,
            query: QueryMsg,
            execute: ExecuteMsg,
            migrate: MigrateMsg,
            sudo: SudoMsg,
        }
        .render();
        let dir = std::path::Path::new("schema").join("phoenix-rotation");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dao-dao-core.json"),
            api.to_string().unwrap() + "\n",
        )
        .unwrap();
    }
}
