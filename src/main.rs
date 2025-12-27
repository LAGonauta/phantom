use clap::Parser;
use tokio::task::LocalSet;

use crate::config::Args;
use crate::proxy::kernel;

mod proto;
mod proxy;
mod config;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    pretty_env_logger::init_timed();

    LocalSet::new().run_until(kernel::start(args)).await
}
