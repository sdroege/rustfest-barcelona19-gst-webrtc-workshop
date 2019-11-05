use rand::prelude::*;

use structopt::StructOpt;

use tokio::prelude::*;

use tungstenite::Message as WsMessage;

use anyhow::{anyhow, bail};

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "wss://webrtc.nirbheek.in:8443")]
    server: String,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::from_args();

    // Connect to the given server
    let url = url::Url::parse(&args.server)?;
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await?;

    println!("connected");

    // Say HELLO to the server and see if it replies with HELLO
    let our_id = rand::thread_rng().gen_range(10, 10_000);
    ws.send(WsMessage::Text(format!("HELLO {}", our_id)))
        .await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))??;

    if msg != WsMessage::Text("HELLO".into()) {
        bail!("server didn't say HELLO");
    }

    println!("received {:?}", msg);

    Ok(())
}
