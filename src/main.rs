use structopt::StructOpt;

use tokio::prelude::*;

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "wss://webrtc.nirbheek.in:8443")]
    server: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::from_args();

    println!("args: {:?}", args);

    Ok(())
}
