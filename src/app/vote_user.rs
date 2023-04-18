use crate::messages;
use crate::util::{send_dm, update_user_vote_event};

use anyhow::Result;
use log::{error,info};
use mostro_core::order::Order;
use mostro_core::{Action, Content, Message, Review};
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
        if ev.is_some() { answers_requests.push(ev.unwrap())}
    }
    if answers_requests.is_empty() { return None };
    
    answers_requests.sort_by(|a,b| a.created_at.cmp(&b.created_at));
    Some(answers_requests[0].clone())
}

pub async fn requests_relay(client: Client, relay: (Url, Relay), filters: Filter) -> Option<Event> {
    let relrequest = get_nip_33_event(&relay.1, vec![filters.clone()], client);

    // Buffer vector
    let mut res: Option<Event> = None;

    // Using a timeout of 3 seconds to avoid unresponsive relays to block the loop forever.
    if let Ok(rx) = timeout(Duration::from_secs(3), relrequest).await {
        match rx {
            Some(m) => { res = Some(m) },
            None => { res = None; info!("No requested events found on relay {}", relay.0.to_string()); },
        }
    }
    res
}

pub async fn get_nip_33_event(
    relay: &Relay,
    filters: Vec<Filter>,
    client: Client,
) -> Option<Event> {

    let mut ev: Option<Event> = None;

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
                        ev = None
                        // break;
                    }
                }
                _ => (),
            };
        }
    }

    // Unsubscribe
    relay.send_msg(ClientMessage::close(id), false).await;

    ev
}

pub async fn get_counterpart_reputation(user : &String , my_keys : &Keys, client : &Client) -> Option<Review>{
      // Request NIP33 of the counterparts 
      let tag = format!("\"#d\" : \"{}\"", user);
      let filter = Filter::new().author(my_keys.public_key()).hashtag(tag);
      let event_nip33 = send_relays_requests(client,filter).await;

          
      event_nip33.as_ref()?;

      let reputation = Review::from_json(&event_nip33.unwrap().content).unwrap();

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
    let mut counterpart : String = String::new();
    let mut buyer_voting : bool = false;
    let mut seller_voting : bool = false;

    if  message_sender == buyer {counterpart = seller; buyer_voting = true}
    else if message_sender == seller {counterpart = buyer; seller_voting = true};
    

    // if counterpart.is_none() { return anyhow::Error::new(_) };

    // let counterpart = counterpart.unwrap(); 

    // Check if content of Peer is the same of counterpart
    let mut vote = 0;

    if let Content::Peer(p) = msg.content.unwrap(){ 
        if counterpart != p.pubkey {
            let text_message = messages::cant_do();
            // We create a Message
            let message = Message::new(
                0,
                Some(order.id),
                Action::CantDo,
                Some(Content::TextMessage(text_message)),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;
        }
        vote = p.vote.unwrap();
    }

    let rep = get_counterpart_reputation(&counterpart, my_keys, client).await;
    //Here we have to update values of the review of the counterpart
    if rep.is_none()
    {   
        let first = Review::new(1, vote, vote, vote, vote);
    }


    let mut reputation= rep.unwrap();
    reputation.total_rating += 1;
    if vote > reputation.max_rate { reputation.max_rate = vote };
    if vote < reputation.min_rate { reputation.min_rate = vote };

    //Going on with calculation 


    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_user_vote_event(&counterpart, buyer_voting, seller_voting, reputation.as_json().unwrap(),order.id, my_keys, client, &pool).await?;

    //Update db with vote flag 
    
    
    
    // We create a Message

    // let message = Message::new(
    //     0,
    //     Some(order.id),
    //     Action::FiatSent,
    //     Some(Content::Peer(peer)),
    // );
    // let message = message.as_json().unwrap();
    // send_dm(client, my_keys, &seller_pubkey, message).await?;
    // // We send a message to buyer to wait
    // let peer = Peer::new(seller_pubkey.to_bech32()?);

    // // We create a Message
    // let message = Message::new(
    //     0,
    //     Some(order.id),
    //     Action::FiatSent,
    //     Some(Content::Peer(peer)),
    // );
    // let message = message.as_json()?;
    // send_dm(client, my_keys, &event.pubkey, message).await?;
    Ok(())
}
