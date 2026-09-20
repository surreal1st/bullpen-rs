//! S10-09: `propose_tool` — bots write tools for Josh to approve.

use std::sync::{Arc, Mutex};

use store::Db;

use crate::bot_tools;

pub fn spec() -> model::ToolSpec {
    bot_tools::spec()
}

pub async fn run(
    db: &Arc<Mutex<Db>>,
    db_path: &str,
    data_dir: &str,
    bot_id: &str,
    args: &str,
) -> String {
    bot_tools::run_propose_tool(db, db_path, data_dir, bot_id, args).await
}
