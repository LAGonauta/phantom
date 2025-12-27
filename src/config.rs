use clap::Parser;

pub const MAX_MTU: usize = 1472;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    #[arg(short = 's', long = "server")]
    pub server: String,

    #[arg(long = "timeout", default_value_t = 60)]
    pub timeout: u64,

    #[arg(long = "remove_ports")]
    pub remove_ports: bool,
}