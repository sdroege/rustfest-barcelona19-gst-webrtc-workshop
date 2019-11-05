use std::sync::{Arc, Mutex, Weak};

use rand::prelude::*;

use structopt::StructOpt;

use tokio::prelude::*;
use tokio::sync::mpsc;

use tungstenite::Error as WsError;
use tungstenite::Message as WsMessage;

use gst::gst_element_error;
use gst::prelude::*;

use serde_derive::{Deserialize, Serialize};

use anyhow::{anyhow, bail, Context};

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

// upgrade weak reference or return
#[macro_export]
macro_rules! upgrade_weak {
    ($x:ident, $r:expr) => {{
        match $x.upgrade() {
            Some(o) => o,
            None => return $r,
        }
    }};
    ($x:ident) => {
        upgrade_weak!($x, ())
    };
}

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "wss://webrtc.nirbheek.in:8443")]
    server: String,
    #[structopt(short, long)]
    peer_id: u32,
}

// JSON messages we communicate with
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

// Strong reference to our application state
#[derive(Debug, Clone)]
struct App(Arc<AppInner>);

// Weak reference to our application state
#[derive(Debug, Clone)]
struct AppWeak(Weak<AppInner>);

// Actual application state
#[derive(Debug)]
struct AppInner {
    args: Args,
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    send_msg_tx: Mutex<mpsc::UnboundedSender<WsMessage>>,
}

// To be able to access the App's fields directly
impl std::ops::Deref for App {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.0
    }
}

impl AppWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(App)
    }
}

impl App {
    // Downgrade the strong reference to a weak reference
    fn downgrade(&self) -> AppWeak {
        AppWeak(Arc::downgrade(&self.0))
    }

    fn new(
        args: Args,
    ) -> Result<
        (
            Self,
            impl Stream<Item = gst::Message>,
            impl Stream<Item = WsMessage>,
        ),
        anyhow::Error,
    > {
        // Create the GStreamer pipeline
        let pipeline = gst::parse_launch(
        "videotestsrc pattern=ball is-live=true ! vp8enc deadline=1 ! rtpvp8pay pt=96 ! webrtcbin. \
         audiotestsrc is-live=true ! opusenc ! rtpopuspay pt=97 ! webrtcbin. \
         webrtcbin name=webrtcbin"
    )?;

        // Downcast from gst::Element to gst::Pipeline
        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("not a pipeline");

        // Get access to the webrtcbin by name
        let webrtcbin = pipeline
            .get_by_name("webrtcbin")
            .expect("can't find webrtcbin");

        // Set some properties on webrtcbin
        webrtcbin.set_property_from_str("stun-server", STUN_SERVER);
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        let bus = pipeline.get_bus().unwrap();

        // Send our bus messages via a futures channel to be handled asynchronously
        let (send_gst_msg_tx, send_gst_msg_rx) = mpsc::unbounded_channel::<gst::Message>();
        let send_gst_msg_tx = Mutex::new(send_gst_msg_tx);
        bus.set_sync_handler(move |_, msg| {
            let _ = send_gst_msg_tx.lock().unwrap().try_send(msg.clone());
            gst::BusSyncReply::Drop
        });

        // Channel for outgoing WebSocket messages from other threads
        let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded_channel::<WsMessage>();

        // Asynchronously set the pipeline to Playing
        pipeline.call_async(|pipeline| {
            pipeline
                .set_state(gst::State::Playing)
                .expect("Couldn't set pipeline to Playing");
        });

        let app = App(Arc::new(AppInner {
            args,
            pipeline,
            webrtcbin,
            send_msg_tx: Mutex::new(send_ws_msg_tx),
        }));

        // Connect to on-negotiation-needed to handle sending an Offer
        let app_clone = app.downgrade();
        app.webrtcbin
            .connect("on-negotiation-needed", false, move |values| {
                let _webrtc = values[0].get::<gst::Element>().unwrap();

                let app = upgrade_weak!(app_clone, None);
                if let Err(err) = app.on_negotiation_needed() {
                    gst_element_error!(
                        app.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to negotiate: {:?}", err)
                    );
                }

                None
            })
            .unwrap();

        // Asynchronously set the pipeline to Playing
        app.pipeline.call_async(|pipeline| {
            // If this fails, post an error on the bus so we exit
            if pipeline.set_state(gst::State::Playing).is_err() {
                gst_element_error!(
                    pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to set pipeline to Playing")
                );
            }
        });

        Ok((app, send_gst_msg_rx, send_ws_msg_rx))
    }

    // Handle WebSocket messages, both our own as well as WebSocket protocol messages
    fn handle_websocket_message(&self, msg: &str) -> Result<(), anyhow::Error> {
        println!("received message {}", msg);

        Ok(())
    }

    // Handle GStreamer messages coming from the pipeline
    fn handle_pipeline_message(&self, message: &gst::Message) -> Result<(), anyhow::Error> {
        use gst::message::MessageView;

        match message.view() {
            MessageView::Error(err) => bail!(
                "Error from element {}: {} ({})",
                err.get_src()
                    .map(|s| String::from(s.get_path_string()))
                    .unwrap_or_else(|| String::from("None")),
                err.get_error(),
                err.get_debug().unwrap_or_else(|| String::from("None")),
            ),
            MessageView::Warning(warning) => {
                println!("Warning: \"{}\"", warning.get_debug().unwrap());
            }
            _ => (),
        }

        Ok(())
    }

    // Whenever webrtcbin tells us that (re-)negotiation is needed, simply ask
    // for a new offer SDP from webrtcbin without any customization and then
    // asynchronously send it to the peer via the WebSocket connection
    fn on_negotiation_needed(&self) -> Result<(), anyhow::Error> {
        println!("starting negotiation");

        let app_clone = self.downgrade();
        let promise = gst::Promise::new_with_change_func(move |promise| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_offer_created(promise) {
                gst_element_error!(
                    app.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to send SDP offer: {:?}", err)
                );
            }
        });

        self.webrtcbin
            .emit("create-offer", &[&None::<gst::Structure>, &promise])
            .unwrap();

        Ok(())
    }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_offer_created(&self, promise: &gst::Promise) -> Result<(), anyhow::Error> {
        let reply = match promise.wait() {
            gst::PromiseResult::Replied => promise.get_reply().unwrap(),
            err => {
                bail!("Offer creation future got no reponse: {:?}", err);
            }
        };

        let offer = reply
            .get_value("offer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit("set-local-description", &[&offer, &None::<gst::Promise>])
            .unwrap();

        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "offer".to_string(),
            sdp: offer.get_sdp().as_text().unwrap(),
        })
        .unwrap();

        println!("sending SDP offer to peer: {}", message);

        self.send_msg_tx
            .lock()
            .unwrap()
            .try_send(WsMessage::Text(message))
            .with_context(|| format!("Failed to send SDP offer"))?;

        Ok(())
    }
}

// Make sure to shut down the pipeline when it goes out of scope
// to release any system resources
impl Drop for AppInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

async fn run(
    args: Args,
    ws: impl Sink<WsMessage, Error = WsError> + Stream<Item = Result<WsMessage, WsError>>,
) -> Result<(), anyhow::Error> {
    // Split the websocket into the Sink and Stream
    let (mut ws_sink, ws_stream) = ws.split();
    // Fuse the Stream, required for the select macro
    let mut ws_stream = ws_stream.fuse();

    // Create our application state
    let (app, send_gst_msg_rx, send_ws_msg_rx) = App::new(args)?;

    let mut send_gst_msg_rx = send_gst_msg_rx.fuse();
    let mut send_ws_msg_rx = send_ws_msg_rx.fuse();

    // And now let's start our message loop
    loop {
        let ws_msg = futures::select! {
            // Handle the WebSocket messages here
            ws_msg = ws_stream.select_next_some() => {
                match ws_msg? {
                    WsMessage::Close(_) => {
                        println!("peer disconnected");
                        break
                    },
                    WsMessage::Ping(data) => Some(WsMessage::Pong(data)),
                    WsMessage::Pong(_) => None,
                    WsMessage::Binary(_) => None,
                    WsMessage::Text(text) => {
                        app.handle_websocket_message(&text)?;
                        None
                    },
                }
            },
            // Pass the GStreamer messages to the application control logic
            gst_msg = send_gst_msg_rx.select_next_some() => {
                app.handle_pipeline_message(&gst_msg)?;
                None
            },
            // Handle WebSocket messages we created asynchronously
            // to send them out now
            ws_msg = send_ws_msg_rx.select_next_some() => Some(ws_msg),
            // Once we're done, break the loop and return
            complete => break,
        };

        // If there's a message to send out, do so now
        if let Some(ws_msg) = ws_msg {
            ws_sink.send(ws_msg).await?;
        }
    }

    Ok(())
}

// Check if all GStreamer plugins we require are available
fn check_plugins() -> Result<(), anyhow::Error> {
    let needed = [
        "videotestsrc",
        "audiotestsrc",
        "videoconvert",
        "audioconvert",
        "autodetect",
        "opus",
        "vpx",
        "webrtc",
        "nice",
        "dtls",
        "srtp",
        "rtpmanager",
        "rtp",
        "playback",
        "videoscale",
        "audioresample",
    ];

    let registry = gst::Registry::get();
    let missing = needed
        .iter()
        .filter(|n| registry.find_plugin(n).is_none())
        .cloned()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        bail!("Missing plugins: {:?}", missing);
    } else {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Initialize GStreamer first
    gst::init()?;

    check_plugins()?;

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

    // Join the given session
    ws.send(WsMessage::Text(format!("SESSION {}", args.peer_id)))
        .await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))??;

    if msg != WsMessage::Text("SESSION_OK".into()) {
        bail!("server error: {:?}", msg);
    }

    // All good, let's run our message loop
    run(args, ws).await
}
