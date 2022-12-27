use std::env;
use std::str::FromStr;

use bitcoincore_rpc::bitcoin::hashes::hex::{FromHex, ToHex};
use bitcoincore_rpc::bitcoin::BlockHash;
use bitcoincore_rpc::{Client, RpcApi};
use chrono::Utc;
use log::{debug, error, info};
use num_format::{Locale, ToFormattedString};
use secp256k1::hashes::sha256;
use secp256k1::{schnorr, KeyPair, Message, Secp256k1, SecretKey, XOnlyPublicKey};
use serde::Serialize;
use serde_json::json;
use websocket::ClientBuilder;
use zmq::SUB;

#[derive(Serialize)]
struct Event {
    id: String,
    pubkey: XOnlyPublicKey,
    created_at: i64,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: String,
    sig: schnorr::Signature,
}

impl Event {
    fn new(secret_key: &SecretKey, kind: u64, tags: Vec<Vec<String>>, content: String) -> Self {
        let created_at = Utc::now().timestamp();
        let secp = Secp256k1::new();
        let keypair = KeyPair::from_secret_key(&secp, secret_key);
        let pubkey = keypair.x_only_public_key().0;
        let serialized_event = json!([0, pubkey, created_at, kind, json!(tags), content]);
        debug!("{}", serialized_event);
        let msg =
            Message::from_hashed_data::<sha256::Hash>(serialized_event.to_string().as_bytes());
        let id = msg.to_string();
        let sig = secp.sign_schnorr(&msg, &keypair);

        Self {
            id,
            pubkey,
            created_at,
            kind,
            tags,
            content,
            sig,
        }
    }
}

#[derive(Serialize)]
struct Report {
    sats_transferred: u64,
    usd_transferred: f64,
    block_height: u64,
    block_hash: String,
    exchange_rate: f64,
}

fn get_value_report_for_block(blockhash: &BlockHash) -> Report {
    let rpc = Client::new(
        &env::var("RPC_HOST").unwrap(),
        bitcoincore_rpc::Auth::UserPass(
            env::var("RPC_USER").unwrap(),
            env::var("RPC_PASSWORD").unwrap(),
        ),
    )
    .expect("Could not create RPC client");
    info!("Looking at block hash: {}", blockhash);
    let block = rpc.get_block(&blockhash).unwrap();
    let height = block.bip34_block_height().unwrap();
    info!("Got data for block at height {}", height);
    let total: u64 = block
        .txdata
        .iter()
        .filter(|tx| !tx.is_coin_base())
        .map(|tx| {
            tx.output
                .iter()
                .map(|output| output.value)
                .fold(0u64, |acc, x| acc + x)
        })
        .fold(0u64, |acc, x| acc + x);

    let http = reqwest::blocking::Client::new();
    let price_quote: serde_json::Value = http
        .get("https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd")
        .send()
        .unwrap()
        .json()
        .unwrap();

    let price = price_quote
        .get("bitcoin")
        .unwrap()
        .get("usd")
        .unwrap()
        .as_f64()
        .unwrap();

    let btc_transferred = total as f64 / 100_000_000.0;

    let total_usd = btc_transferred * price;

    info!(
        "total transferred (excluding coinbase): {} bitcoin or ${:.2}",
        btc_transferred, total_usd
    );

    let report = Report {
        block_hash: blockhash.to_string(),
        exchange_rate: price,
        block_height: height,
        sats_transferred: total,
        usd_transferred: f64::trunc(total_usd * 100.0) / 100.0,
    };

    return report;
}

fn main() {
    env_logger::init();
    let ctx = zmq::Context::new();
    let socket = ctx.socket(SUB).expect("Couldn't create zmq socket");
    socket
        .connect("tcp://127.0.0.1:28334")
        .expect("Couldn't connect zmq socket");
    socket.set_subscribe(b"").expect("Could not set_subscribe");

    let secret_key = SecretKey::from_str(&env::var("NOSTR_PRIVKEY").unwrap()).unwrap();

    info!("Starting listen loop");
    loop {
        let message = socket
            .recv_multipart(0)
            .expect("Could not receive multipart message");
        let topic = std::str::from_utf8(message.first().unwrap()).unwrap();
        debug!("Got a message with topic {}", topic);
        if topic == "hashblock" {
            debug!("Got a new hashblock!");
            let blockhash = BlockHash::from_hex(&message[1].to_hex()).unwrap();
            let report = get_value_report_for_block(&blockhash);
            let msg = format!("Block {} was just confirmed. The total value of all the non-coinbase outputs was {} sats, or ${}",
                              report.block_height,
                              report.sats_transferred.to_formatted_string(&Locale::en),
                              (report.usd_transferred as u64).to_formatted_string(&Locale::en));
            let event = Event::new(&secret_key, 1, Vec::new(), msg);
            let event_json = json!(event).to_string();
            debug!("{}", event_json);

            let event_msg = json!(["EVENT", event]).to_string();
            debug!("{}", event_msg);
            let message = websocket::Message::text(event_msg);
            for relay in vec![
                "wss://relay.damus.io",
                "wss://nostr.zebedee.cloud",
                "wss://relay.nostr.ch",
                "wss://nostr-pub.wellorder.net",
                "wss://nostr-pub.semisol.dev",
                "wss://nostr.oxtr.dev",
            ] {
                match publish_to_relay(relay, &message) {
                    Ok(_) => info!("sent message to {}", relay),
                    Err(e) => error!("{}", e),
                };
            }
        }
    }
}

fn publish_to_relay(relay: &str, message: &websocket::Message) -> Result<(), String> {
    let mut client = ClientBuilder::new(relay)
        .map_err(|err| format!("Could not create client: {}", err.to_string()))?
        .connect(None)
        .map_err(|err| format!("Could not connect to relay {}: {}", relay, err.to_string()))?;
    client
        .send_message(message)
        .map_err(|err| format!("could not send message to relay: {}", err.to_string()))?;
    Ok(())
}
