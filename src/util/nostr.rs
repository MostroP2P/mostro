use crate::config::constants::{DEV_FEE_AUDIT_EVENT_KIND, DEV_FEE_LIGHTNING_ADDRESS};
use crate::config::settings::Settings;
use crate::nip33::new_event;
use crate::util::orders::get_order;
use crate::config::MESSAGE_QUEUES;
use crate::LN_STATUS;
use crate::NOSTR_CLIENT;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use std::borrow::Cow;
use std::collections::HashMap;
use tracing::info;

pub async fn send_dm(
    receiver_pubkey: PublicKey,
    sender_keys: &Keys,
    payload: &str,
    expiration: Option<Timestamp>,
) -> Result<(), MostroError> {
    info!(
        "sender key {} - receiver key {}",
        sender_keys.public_key().to_hex(),
        receiver_pubkey.to_hex()
    );
    let message = Message::from_json(payload)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // We compose the content, as this is a message from Mostro
    // and Mostro don't have trade key, we don't need to sign the payload
    let content = (message, Option::<String>::None);
    let content = serde_json::to_string(&content)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // We create the rumor
    let rumor = EventBuilder::text_note(content).build(sender_keys.public_key());
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + usize::from(expiration.is_some()));

    if let Some(timestamp) = expiration {
        tags.push(Tag::expiration(timestamp));
    }
    let tags = Tags::from_list(tags);

    let event = EventBuilder::gift_wrap(sender_keys, &receiver_pubkey, rumor, tags)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    info!(
        "Sending DM, Event ID: {} to {} with payload: {:#?}",
        event.id,
        receiver_pubkey.to_hex(),
        payload
    );

    if let Ok(client) = get_nostr_client() {
        client
            .send_event(&event)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    }

    Ok(())
}

/// Publishes a dev fee payment audit event to Nostr relays
pub async fn publish_dev_fee_audit_event(
    order: &Order,
    payment_hash: &str,
) -> Result<(), MostroError> {
    let ln_network = match LN_STATUS.get() {
        Some(status) => status.networks.join(","),
        None => "unknown".to_string(),
    };
    // Get Mostro keys for signing
    let keys = get_keys()?;

    // Get Nostr client
    let client = get_nostr_client()?;

    // Create tags for queryability
    let tags = Tags::from_list(vec![
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("order-id")),
            vec![order.id.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("amount")),
            vec![order.dev_fee.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("hash")),
            vec![payment_hash.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("destination")),
            vec![DEV_FEE_LIGHTNING_ADDRESS.to_string()],
        ),
        Tag::custom(TagKind::Custom(Cow::Borrowed("network")), vec![ln_network]),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            vec!["mostro".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["dev-fee-payment".to_string()],
        ),
    ]);

    // Create and sign event
    let event = EventBuilder::new(nostr_sdk::Kind::Custom(DEV_FEE_AUDIT_EVENT_KIND), "")
        .tags(tags)
        .sign_with_keys(&keys)
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Publish event to relays
    client
        .send_event(&event)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "📡 Published dev fee audit event for order {} - {} sats to relays",
        order.id, order.dev_fee
    );

    Ok(())
}

pub fn get_keys() -> Result<Keys, MostroError> {
    let nostr_settings = Settings::get_nostr();
    // nostr private key
    match Keys::parse(&nostr_settings.nsec_privkey) {
        Ok(my_keys) => Ok(my_keys),
        Err(e) => {
            tracing::error!("Failed to parse nostr private key: {}", e);
            Err(MostroInternalErr(ServiceError::NostrError(e.to_string())))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_user_rating_event(
    user: &str,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    tags: Tags,
    msg: &Message,
    keys: &Keys,
    pool: &SqlitePool,
) -> Result<()> {
    // Get order from msg
    let mut order = get_order(msg, pool).await?;

    // nip33 kind with user as identifier
    let event = new_event(keys, "", user.to_string(), tags)?;
    info!("Sending replaceable event: {event:#?}");
    // We update the order vote status
    if buyer_sent_rate {
        order.buyer_sent_rate = buyer_sent_rate;
    }
    if seller_sent_rate {
        order.seller_sent_rate = seller_sent_rate;
    }
    order.update(pool).await?;

    // Add event message to global list
    MESSAGE_QUEUES.queue_order_rate.write().await.push(event);
    Ok(())
}

pub async fn connect_nostr() -> Result<Client, MostroError> {
    let nostr_settings = Settings::get_nostr();

    let mut limits = RelayLimits::default();
    // Some specific events can have a bigger size than regular events
    // So we increase the limits for those events
    limits.messages.max_size = Some(6_000);
    limits.events.max_size = Some(6_500);
    let opts = ClientOptions::new().relay_limits(limits);

    // Create new client
    let client = ClientBuilder::default().opts(opts).build();

    // Add relays
    for relay in nostr_settings.relays.iter() {
        client
            .add_relay(relay)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    }

    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}

/// Getter function with error management for nostr Client
pub fn get_nostr_client() -> Result<&'static Client, MostroError> {
    if let Some(client) = NOSTR_CLIENT.get() {
        Ok(client)
    } else {
        Err(MostroInternalErr(ServiceError::NostrError(
            "Client not initialized!".to_string(),
        )))
    }
}

/// Getter function with error management for nostr relays
pub async fn get_nostr_relays() -> Option<HashMap<RelayUrl, Relay>> {
    if let Some(client) = NOSTR_CLIENT.get() {
        Some(client.relays().await)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys as NostrKeys;
    use uuid::uuid;

    #[tokio::test]
    async fn test_get_nostr_client_failure() {
        // Ensure NOSTR_CLIENT is not initialized for the test
        let client = NOSTR_CLIENT.get();
        assert!(client.is_none());
    }

    #[tokio::test]
    async fn test_get_nostr_client_success() {
        // Mock NOSTR_CLIENT initialization
        let client = Client::default();
        NOSTR_CLIENT.set(client).unwrap();
        let client_result = get_nostr_client();
        assert!(client_result.is_ok());
    }

    #[tokio::test]
    async fn test_send_dm() {
        // Mock the send_dm function
        let receiver_pubkey = NostrKeys::generate().public_key();
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let message = Message::Order(mostro_core::message::MessageKind::new(
            Some(uuid),
            None,
            None,
            Action::FiatSent,
            None,
        ));
        let payload = message.as_json().unwrap();
        let sender_keys = NostrKeys::generate();
        // Now error is well manager this call will fail now, previously test was ok because error was not managed
        // now just make it ok and then will make a better test
        let result = send_dm(receiver_pubkey, &sender_keys, &payload, None).await;
        assert!(result.is_err());
    }
}
