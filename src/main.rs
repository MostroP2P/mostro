use nostr::util::time::timestamp;
use nostr::{Kind, KindBase, SubscriptionFilter};
use nostr_sdk::nostr::Keys;
use nostr_sdk::{Client, RelayPoolNotifications};

mod types;
mod util;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // From Bech32
    use nostr::key::FromBech32;
    // nostr private key
    let nsec1privkey = "nsec1...";
    let my_keys = Keys::from_bech32(nsec1privkey)?;

    // Create new client
    let mut client = Client::new(&my_keys);

    // Add relays
    client.add_relay("wss://relay.grunch.dev", None)?;
    client.add_relay("wss://relay.damus.io", None)?;
    client.add_relay("wss://nostr.openchain.fr", None)?;

    // Connect to relays and keep connection alive
    client.connect().await?;

    let subscription = SubscriptionFilter::new()
        .pubkey(my_keys.public_key())
        .since(timestamp());

    client.subscribe(vec![subscription]).await?;

    client
        .handle_notifications(|notification| {
            if let RelayPoolNotifications::ReceivedEvent(event) = notification {
                if event.kind == Kind::Base(KindBase::EncryptedDirectMessage) {
                    util::handle_dm(&my_keys, &event);
                } else {
                    println!("{:#?}", event);
                }
            }

            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use crate::types::{Message, Order};

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"payment_request":null,"created_at":1640839235}"#;
        let order = Order::from_json(&sample_order).unwrap();
        let json_order = order.to_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message =
            r#"{"version":0,"action":"PaymentRequest","content":{"PaymentRequest":"lnbc1..."}}"#;
        let message = Message::from_json(&sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.to_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"version":0,"action":"PaymentRequest","content":{"Order":{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"payment_request":null,"created_at":1640839235}}}"#;
        let message = Message::from_json(&sample_message).unwrap();
        assert!(!message.verify());
    }
}
