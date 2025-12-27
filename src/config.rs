use clap::Parser;

pub const MAX_MTU: usize = 1472;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    #[arg(short = 's', long = "server")]
    pub server: String,

    #[arg(long = "timeout", default_value_t = 60)]
    pub timeout: u64,

    #[arg(long = "remove-ports")]
    pub remove_ports: bool,

    #[arg(long = "disable-kernel-proxy", default_value_t = false, help = "Disables kernel-based proxy even if supported")]
    pub disable_kernel_proxy: bool,
}