mod nostr_engine;

use crate::nostr_engine::NostrEngine;
use core_types::nostr::NostrProfile;
use diesel::r2d2::ConnectionManager;
use diesel::PgConnection;
use msgs::Message;
use nostr_sdk::prelude::{Event, FromPkStr, Keys, Kind, SubscriptionFilter, Timestamp};
use nostr_sdk::{Client, RelayPoolNotification};
use serde::{Deserialize, Serialize};
use slog as log;
use slog::Logger;
use std::collections::HashMap;
use std::net::SocketAddr;
use utils::xlogging::LoggingSettings;
use utils::xzmq::ZmqSocket;

const PROFILE_REQUEST_TIMEOUT_MS: u64 = 100;

type DbPool = diesel::r2d2::Pool<ConnectionManager<PgConnection>>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NostrEngineSettings {
    pub psql_url: String,
    pub nostr_bank_push_address: String,
    pub nostr_bank_pull_address: String,
    pub nostr_private_key: String,
    pub nostr_engine_logging_settings: LoggingSettings,
}

#[derive(Debug)]
pub enum NostrEngineEvent {
    NostrProfileUpdate(Box<NostrProfileUpdate>),
    LndhubxMessage(Message),
}

#[derive(Debug)]
pub struct NostrProfileUpdate {
    pub pubkey: String,
    pub created_at_epoch_ms: u64,
    pub received_at_epoch_ms: u64,
    pub nostr_profile: NostrProfile,
}

#[derive(Deserialize, Debug)]
pub struct Nip05Response {
    pub names: HashMap<String, String>,
}

pub fn spawn_profile_subscriber(
    nostr_engine_keys: Keys,
    relays: Vec<(String, Option<SocketAddr>)>,
    subscribe_since_epoch_seconds: u64,
    events_tx: tokio::sync::mpsc::Sender<NostrEngineEvent>,
    logger: Logger,
) {
    tokio::spawn(async move {
        let nostr_client = Client::new(&nostr_engine_keys);
        nostr_client.add_relays(relays).await.unwrap();
        nostr_client.connect().await;

        let since_seconds = Timestamp::from(subscribe_since_epoch_seconds);
        let subscription = SubscriptionFilter::new().kind(Kind::Metadata).since(since_seconds);
        nostr_client.subscribe(vec![subscription]).await;
        loop {
            let mut notifications = nostr_client.notifications();
            while let Ok(notification) = notifications.recv().await {
                if let RelayPoolNotification::Event(_url, event) = notification {
                    match try_profile_update_from_event(&event).await {
                        Some(profile_update) => {
                            let msg = NostrEngineEvent::NostrProfileUpdate(Box::new(profile_update));
                            if let Err(err) = events_tx.send(msg).await {
                                log::error!(
                                    logger,
                                    "Failed to send nostr profile update to events channel, error: {:?}",
                                    err
                                );
                            }
                        }
                        None => {
                            log::error!(logger, "Failed to deserialize {} into nostr profile", &event.content);
                        }
                    }
                }
            }
        }
    });
}

pub fn spawn_events_handler(
    nostr_engine_keys: Keys,
    relays: Vec<(String, Option<SocketAddr>)>,
    mut events_rx: tokio::sync::mpsc::Receiver<NostrEngineEvent>,
    response_socket: ZmqSocket,
    db_pool: DbPool,
    logger: Logger,
) {
    tokio::spawn(async move {
        let mut nostr_engine = NostrEngine::new(nostr_engine_keys, relays, response_socket, db_pool, logger).await;
        while let Some(event) = events_rx.recv().await {
            nostr_engine.process_event(&event).await;
        }
    });
}

async fn get_user_profile(client: &Client, pubkey: &str) -> Option<NostrProfileUpdate> {
    let keys = Keys::from_pk_str(pubkey).ok()?;

    let subscription = SubscriptionFilter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let timeout = std::time::Duration::from_millis(PROFILE_REQUEST_TIMEOUT_MS);
    let events = client.get_events_of(vec![subscription], Some(timeout)).await.ok()?;

    match events.first() {
        Some(event) => try_profile_update_from_event(event).await,
        None => None,
    }
}

async fn send_nostr_private_msg(client: &Client, pubkey: &str, text: &str) {
    let keys = Keys::from_pk_str(pubkey).unwrap();
    client.send_direct_msg(keys.public_key(), text).await.unwrap();
}

async fn verify_nip05(pubkey: String, nip05: String) -> Option<bool> {
    if let Some((local_part, domain)) = nip05.split_once('@') {
        let url = format!("https://{domain}/.well-known/nostr.json?name={local_part}");
        let body = reqwest::get(&url).await.ok()?.text().await.ok()?;
        let nip_verification = serde_json::from_str::<Nip05Response>(&body).ok()?;
        return nip_verification
            .names
            .get(local_part)
            .map(|response_pubkey| response_pubkey == &pubkey);
    }
    None
}

async fn try_profile_update_from_event(event: &Event) -> Option<NostrProfileUpdate> {
    if event.kind == Kind::Metadata {
        let mut nostr_profile = serde_json::from_str::<NostrProfile>(&event.content).ok()?;
        let pubkey = event.pubkey.to_string();
        let created_at_epoch_ms = 1000 * event.created_at.as_u64();
        let received_at_epoch_ms = utils::time::time_now();
        let verified = if let Some(nip05) = nostr_profile.nip05().as_ref() {
            verify_nip05(pubkey.clone(), nip05.clone()).await
        } else {
            None
        };
        nostr_profile.set_nip05_verified(verified);
        let profile_update = NostrProfileUpdate {
            pubkey,
            created_at_epoch_ms,
            received_at_epoch_ms,
            nostr_profile,
        };
        return Some(profile_update);
    }
    None
}
