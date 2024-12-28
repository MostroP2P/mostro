use crate::nip33::{dispute_from_tags, order_from_tags};

use anyhow::{Error, Result};
use base64::engine::general_purpose;
use base64::Engine;
use dotenvy::var;
use log::{error, info};
use mostro_core::dispute::Dispute;
use mostro_core::message::Message;
use mostro_core::order::Kind as MostroKind;
use mostro_core::order::{SmallOrder, Status};
use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nip44::v2::{decrypt_to_bytes, encrypt_to_bytes, ConversationKey};
use nostr_sdk::prelude::*;
use std::thread::sleep;
use std::time::Duration;
use std::{fs, path::Path};

pub async fn send_dm(
    client: &Client,
    identity_keys: Option<&Keys>,
    trade_keys: &Keys,
    receiver_pubkey: &PublicKey,
    payload: String,
    expiration: Option<Timestamp>,
    to_user: bool,
) -> Result<()> {
    let pow: u8 = var("POW").unwrap_or('0'.to_string()).parse().unwrap();
    let event = if to_user {
        // Derive conversation key
        let ck = ConversationKey::derive(trade_keys.secret_key(), receiver_pubkey);
        // Encrypt payload
        let encrypted_content = encrypt_to_bytes(&ck, payload.as_bytes())?;
        // Encode with base64
        let b64decoded_content = general_purpose::STANDARD.encode(encrypted_content);
        // Compose builder
        EventBuilder::new(Kind::PrivateDirectMessage, b64decoded_content)
            .pow(pow)
            .tag(Tag::public_key(*receiver_pubkey))
            .sign_with_keys(trade_keys)?
    } else {
        let identity_keys = identity_keys
            .ok_or_else(|| Error::msg("identity_keys required when to_user is false"))?;

        let message = Message::from_json(&payload).unwrap();
        // We sign the message
        let sig = message.get_inner_message_kind().sign(trade_keys);
        // We compose the content
        let content = (message, sig);
        let content = serde_json::to_string(&content).unwrap();
        // We create the rumor
        let rumor = EventBuilder::text_note(content)
            .pow(pow)
            .build(trade_keys.public_key());
        let mut tags: Vec<Tag> = Vec::with_capacity(1 + usize::from(expiration.is_some()));
        tags.push(Tag::public_key(*receiver_pubkey));

        if let Some(timestamp) = expiration {
            tags.push(Tag::expiration(timestamp));
        }
        let tags = Tags::new(tags);

        EventBuilder::gift_wrap(identity_keys, receiver_pubkey, rumor, tags).await?
    };

    info!("Sending event: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}

pub async fn connect_nostr() -> Result<Client> {
    let my_keys = Keys::generate();

    let relays = var("RELAYS").expect("RELAYS is not set");
    let relays = relays.split(',').collect::<Vec<&str>>();
    // Create new client
    let client = Client::new(my_keys);
    // Add relays
    for r in relays.into_iter() {
        client.add_relay(r).await?;
    }
    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}

pub async fn send_message_sync(
    client: &Client,
    identity_keys: Option<&Keys>,
    trade_keys: &Keys,
    receiver_pubkey: PublicKey,
    message: Message,
    wait_for_dm: bool,
    to_user: bool,
) -> Result<Vec<(Message, u64)>> {
    let message_json = message.as_json()?;
    // Send dm to receiver pubkey
    println!(
        "SENDING DM with trade keys: {:?}",
        trade_keys.public_key().to_hex()
    );
    send_dm(
        client,
        identity_keys,
        trade_keys,
        &receiver_pubkey,
        message_json,
        None,
        to_user,
    )
    .await?;
    // FIXME: This is a hack to wait for the DM to be sent
    sleep(Duration::from_secs(2));

    let dm: Vec<(Message, u64)> = if wait_for_dm {
        get_direct_messages(client, trade_keys, 15, to_user).await
    } else {
        Vec::new()
    };

    Ok(dm)
}

pub async fn get_direct_messages(
    client: &Client,
    my_key: &Keys,
    since: i64,
    from_user: bool,
) -> Vec<(Message, u64)> {
    // We use a fake timestamp to thwart time-analysis attacks
    let fake_since = 2880;
    let fake_since_time = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::minutes(fake_since))
        .unwrap()
        .timestamp() as u64;

    let fake_timestamp = Timestamp::from(fake_since_time);
    let filters = if from_user {
        let since_time = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::minutes(since))
            .unwrap()
            .timestamp() as u64;
        let timestamp = Timestamp::from(since_time);
        Filter::new()
            .kind(Kind::PrivateDirectMessage)
            .pubkey(my_key.public_key())
            .since(timestamp)
    } else {
        Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(my_key.public_key())
            .since(fake_timestamp)
    };

    info!("Request events with event kind : {:?} ", filters.kinds);

    let mut direct_messages: Vec<(Message, u64)> = Vec::new();

    if let Ok(mostro_req) = client
        .fetch_events(vec![filters], Duration::from_secs(15))
        .await
    {
        // Buffer vector for direct messages
        // Vector for single order id check - maybe multiple relay could send the same order id? Check unique one...
        let mut id_list = Vec::<EventId>::new();

        for dm in mostro_req.iter() {
            if !id_list.contains(&dm.id) {
                id_list.push(dm.id);
                let created_at: Timestamp;
                let message: Message;
                if from_user {
                    let ck = ConversationKey::derive(my_key.secret_key(), &dm.pubkey);
                    let b64decoded_content =
                        match general_purpose::STANDARD.decode(dm.content.as_bytes()) {
                            Ok(b64decoded_content) => b64decoded_content,
                            Err(_) => {
                                continue;
                            }
                        };
                    // Decrypt
                    let unencrypted_content = decrypt_to_bytes(&ck, &b64decoded_content).unwrap();
                    let message_str =
                        String::from_utf8(unencrypted_content).expect("Found invalid UTF-8");
                    message = Message::from_json(&message_str).unwrap();
                    created_at = dm.created_at;
                } else {
                    let unwrapped_gift = match nip59::extract_rumor(my_key, dm).await {
                        Ok(u) => u,
                        Err(_) => {
                            println!("Error unwrapping gift");
                            continue;
                        }
                    };
                    let (rumor_message, sig): (Message, nostr_sdk::secp256k1::schnorr::Signature) =
                        serde_json::from_str(&unwrapped_gift.rumor.content).unwrap();
                    if !rumor_message
                        .get_inner_message_kind()
                        .verify_signature(unwrapped_gift.rumor.pubkey, sig)
                    {
                        println!("Signature verification failed");
                        continue;
                    }
                    message = rumor_message;
                    created_at = unwrapped_gift.rumor.created_at;
                }
                // Here we discard messages older than the real since parameter
                let since_time = chrono::Utc::now()
                    .checked_sub_signed(chrono::Duration::minutes(30))
                    .unwrap()
                    .timestamp() as u64;
                if created_at.as_u64() < since_time {
                    println!("Discarding message older than since parameter");
                    continue;
                }
                direct_messages.push((message, created_at.as_u64()));
            }
        }
        // Return element sorted by second tuple element ( Timestamp )
        direct_messages.sort_by(|a, b| a.1.cmp(&b.1));
    }

    direct_messages
}

pub async fn get_orders_list(
    pubkey: PublicKey,
    status: Status,
    currency: Option<String>,
    kind: Option<MostroKind>,
    client: &Client,
) -> Result<Vec<SmallOrder>> {
    let since_time = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(7))
        .unwrap()
        .timestamp() as u64;

    let timestamp = Timestamp::from(since_time);

    let filters = Filter::new()
        .author(pubkey)
        .limit(50)
        .since(timestamp)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::Z), vec!["order"])
        .kind(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND));

    info!(
        "Request to mostro id : {:?} with event kind : {:?} ",
        filters.authors, filters.kinds
    );

    // Extracted Orders List
    let mut complete_events_list = Vec::<SmallOrder>::new();
    let mut requested_orders_list = Vec::<SmallOrder>::new();

    // Send all requests to relays
    if let Ok(mostro_req) = client
        .fetch_events(vec![filters], Duration::from_secs(15))
        .await
    {
        // Scan events to extract all orders
        for el in mostro_req.iter() {
            let order = order_from_tags(el.tags.clone());

            if order.is_err() {
                error!("{order:?}");
                continue;
            }
            let mut order = order?;

            info!("Found Order id : {:?}", order.id.unwrap());

            if order.id.is_none() {
                info!("Order ID is none");
                continue;
            }

            if order.kind.is_none() {
                info!("Order kind is none");
                continue;
            }

            if order.status.is_none() {
                info!("Order status is none");
                continue;
            }

            // Get created at field from Nostr event
            order.created_at = Some(el.created_at.as_u64() as i64);

            complete_events_list.push(order.clone());

            if order.status.ne(&Some(status)) {
                continue;
            }

            if currency.is_some() && order.fiat_code.ne(&currency.clone().unwrap()) {
                continue;
            }

            if kind.is_some() && order.kind.ne(&kind) {
                continue;
            }
            // Add just requested orders requested by filtering
            requested_orders_list.push(order);
        }
    }

    // Order all element ( orders ) received to filter - discard disaligned messages
    // if an order has an older message with the state we received is discarded for the latest one
    requested_orders_list.retain(|keep| {
        !complete_events_list
            .iter()
            .any(|x| x.id == keep.id && x.created_at > keep.created_at)
    });
    // Sort by id to remove duplicates
    requested_orders_list.sort_by(|a, b| b.id.cmp(&a.id));
    requested_orders_list.dedup_by(|a, b| a.id == b.id);

    // Finally sort list by creation time
    requested_orders_list.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    Ok(requested_orders_list)
}

pub async fn get_disputes_list(pubkey: PublicKey, client: &Client) -> Result<Vec<Dispute>> {
    let since_time = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(7))
        .unwrap()
        .timestamp() as u64;

    let timestamp = Timestamp::from(since_time);

    let filter = Filter::new()
        .author(pubkey)
        .limit(50)
        .since(timestamp)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::Z), vec!["dispute"])
        .kind(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND));

    // Extracted Orders List
    let mut disputes_list = Vec::<Dispute>::new();

    // Send all requests to relays
    if let Ok(mostro_req) = client
        .fetch_events(vec![filter], Duration::from_secs(15))
        .await
    {
        // Scan events to extract all disputes
        for d in mostro_req.iter() {
            let dispute = dispute_from_tags(d.tags.clone());

            if dispute.is_err() {
                error!("{dispute:?}");
                continue;
            }
            let mut dispute = dispute?;

            info!("Found Dispute id : {:?}", dispute.id);

            // Get created at field from Nostr event
            dispute.created_at = d.created_at.as_u64() as i64;
            disputes_list.push(dispute);
        }
    }

    let buffer_dispute_list = disputes_list.clone();
    // Order all element ( orders ) received to filter - discard disaligned messages
    // if an order has an older message with the state we received is discarded for the latest one
    disputes_list.retain(|keep| {
        !buffer_dispute_list
            .iter()
            .any(|x| x.id == keep.id && x.created_at > keep.created_at)
    });

    // Sort by id to remove duplicates
    disputes_list.sort_by(|a, b| b.id.cmp(&a.id));
    disputes_list.dedup_by(|a, b| a.id == b.id);

    // Finally sort list by creation time
    disputes_list.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    Ok(disputes_list)
}

/// Uppercase first letter of a string.
pub fn uppercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub fn get_mcli_path() -> String {
    let home_dir = dirs::home_dir().expect("Couldn't get home directory");
    let mcli_path = format!("{}/.mcli", home_dir.display());
    if !Path::new(&mcli_path).exists() {
        fs::create_dir(&mcli_path).expect("Couldn't create mostro-cli directory in HOME");
        println!("Directory {} created.", mcli_path);
    }

    mcli_path
}
