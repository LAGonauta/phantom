use clap::Parser;
use tokio::task::LocalSet;

use crate::config::Args;
use crate::proxy::{fallback, kernel};

mod proto;
mod proxy;
mod config;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    pretty_env_logger::init_timed();

    if !args.disable_kernel_proxy && kernel::has_support().await {
        LocalSet::new().run_until(kernel::start(args)).await
    } else {
        LocalSet::new().run_until(fallback::start(args)).await
    }
}
