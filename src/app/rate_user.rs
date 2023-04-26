use crate::messages;
use crate::util::{send_dm, update_user_rating_event};

use anyhow::Result;
use log::{error, info};
use mostro_core::order::Order;
use mostro_core::{Action, Content, Message, Rating};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

pub async fn send_relays_requests(client: &Client, filters: Filter) -> Option<Event> {
    let relays = client.relays().await;

    let relays_requests = relays.len();
    let mut requests: Vec<tokio::task::JoinHandle<Option<Event>>> =
        Vec::with_capacity(relays_requests);
    let mut answers_requests = Vec::with_capacity(relays_requests);

    for relay in relays.into_iter() {
        info!("Requesting to relay : {}", relay.0.as_str());
        // Spawn futures and join them at the end
        requests.push(tokio::spawn(requests_relay(
            client.clone(),
            relay.clone(),
            filters.clone(),
        )));
    }

    // Get answers from relay
    for req in requests {
        let ev = req.await.unwrap();
        if ev.is_some() {
            answers_requests.push(ev.unwrap())
        }
    }
    if answers_requests.is_empty() {
        return None;
    };

    answers_requests.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Some(answers_requests[0].clone())
}

pub async fn requests_relay(client: Client, relay: (Url, Relay), filters: Filter) -> Option<Event> {
    let relrequest = get_nip_33_event(&relay.1, vec![filters.clone()], client);

    // Buffer vector
    let mut res: Option<Event> = None;

    // Using a timeout of 3 seconds to avoid unresponsive relays to block the loop forever.
    if let Ok(rx) = timeout(Duration::from_secs(3), relrequest).await {
        match rx {
            Some(m) => res = Some(m),
            None => {
                res = None;
                info!("No requested events found on relay {}", relay.0.to_string());
            }
        }
    }
    res
}

pub async fn get_nip_33_event(
    relay: &Relay,
    filters: Vec<Filter>,
    client: Client,
) -> Option<Event> {
    // Subscribe
    info!(
        "Subscribing for all mostro orders to relay : {}",
        relay.url().to_string()
    );
    let id = SubscriptionId::new(Uuid::new_v4().to_string());
    let msg = ClientMessage::new_req(id.clone(), filters.clone());

    info!("Message sent : {:?}", msg);

    // Send msg to relay
    relay.send_msg(msg.clone(), false).await.unwrap();

    // Wait notification from relays
    let mut notifications = client.notifications();

    let mut ev = None;

    while let Ok(notification) = notifications.recv().await {
        if let RelayPoolNotification::Message(_, msg) = notification {
            match msg {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    if subscription_id == id {
                        ev = Some(event.as_ref().clone());
                    }
                }
                RelayMessage::EndOfStoredEvents(subscription_id) => {
                    if subscription_id == id {
                        break;
                    }
                }
                _ => (),
            };
        }
    }

    // Unsubscribe
    relay.send_msg(ClientMessage::close(id), false).await.ok()?;

    ev
}

pub async fn get_counterpart_reputation(
    user: &String,
    my_keys: &Keys,
    client: &Client,
) -> Option<Rating> {
    // Request NIP33 of the counterparts

    let filter = Filter::new()
        .author(my_keys.public_key().to_string())
        .kind(Kind::Custom(30000))
        .identifier(user.to_string());
    println!("Filter : {:?}", filter);
    let event_nip33 = send_relays_requests(client, filter).await;

    event_nip33.as_ref()?;

    let reputation = Rating::from_json(&event_nip33.unwrap().content).unwrap();

    Some(reputation)
}

pub async fn update_user_reputation_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("FiatSent: Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Get needed info about users
    let buyer = order.buyer_pubkey.unwrap();
    let seller = order.seller_pubkey.unwrap();
    let message_sender = event.pubkey.to_bech32()?;

    // TODO: send to user a DM with the error
    if order.status != "Success" {
        error!("FiatSent: Order Id {order_id} wrong status");
        return Ok(());
    }
    // Get counterpart pubkey
    let mut counterpart: String = String::new();
    let mut buyer_rating: bool = false;
    let mut seller_rating: bool = false;

    if message_sender == buyer {
        counterpart = seller;
        buyer_rating = true
    } else if message_sender == seller {
        counterpart = buyer;
        seller_rating = true
    };

    // Add a check in case of no counterpart found
    // if counterpart.is_none() { return anyhow::Error::new(_) };

    // Check if content of Peer is the same of counterpart
    let mut rating = 0_f64;

    if let Content::Peer(p) = msg.content.unwrap() {
        if counterpart != p.pubkey {
            let text_message = messages::cant_do();
            // We create a Message
            let message = Message::new(
                0,
                Some(order.id),
                None,
                Action::CantDo,
                Some(Content::TextMessage(text_message)),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;
        }
        rating = p.rating.unwrap();
    }

    // Ask counterpart reputation
    let rep = get_counterpart_reputation(&counterpart, my_keys, client).await;
    //Here we have to update values of the review of the counterpart
    let mut reputation;

    if rep.is_none() {
        reputation = Rating::new(1.0, rating, rating, rating, rating);
    } else {
        // Update user reputation
        //Going on with calculation
        reputation = rep.unwrap();
        reputation.total_reviews += 1.0;
        if rating > reputation.max_rate {
            reputation.max_rate = rating
        };
        if rating < reputation.min_rate {
            reputation.min_rate = rating
        };
        let new_rating =
            reputation.last_rating + (rating - reputation.last_rating) / reputation.total_reviews;
        reputation.last_rating = new_rating;
    }

    // Check if the order is not voted by the message sender and in case update NIP
    let order_to_check_ratings = crate::db::find_order_by_id(pool, order.id).await?;
    if (seller_rating && !order_to_check_ratings.seller_sent_rate)
        || (buyer_rating && !order_to_check_ratings.buyer_sent_rate)
    {
        //Update db with vote flags
        update_user_rating_event(
            &counterpart,
            buyer_rating,
            seller_rating,
            reputation.as_json().unwrap(),
            order.id,
            my_keys,
            client,
            pool,
        )
        .await?;
    }
    Ok(())
}
