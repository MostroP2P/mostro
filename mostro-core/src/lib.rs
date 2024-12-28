pub mod dispute;
pub mod message;
pub mod order;
pub mod rating;
pub mod user;

/// All events broadcasted by Mostro daemon are Parameterized Replaceable Events
/// and the event kind must be between 30000 and 39999
pub const NOSTR_REPLACEABLE_EVENT_KIND: u16 = 38383;
pub const PROTOCOL_VER: u8 = 1;

#[cfg(test)]
mod test {
    use crate::message::{Action, CantDoReason, Message, MessageKind, Payload, Peer};
    use crate::order::{Kind, SmallOrder, Status};
    use nostr_sdk::Keys;
    use uuid::uuid;

    #[test]
    fn test_status_string() {
        assert_eq!(Status::Active.to_string(), "active");
        assert_eq!(Status::CompletedByAdmin.to_string(), "completed-by-admin");
        assert_eq!(Status::FiatSent.to_string(), "fiat-sent");
        assert_ne!(Status::Pending.to_string(), "Pending");
    }

    #[test]
    fn test_kind_string() {
        assert_ne!(Kind::Sell.to_string(), "active");
        assert_eq!(Kind::Sell.to_string(), "sell");
        assert_eq!(Kind::Buy.to_string(), "buy");
        assert_ne!(Kind::Buy.to_string(), "active");
    }

    #[test]
    fn test_order_message() {
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let payload = Payload::Order(SmallOrder::new(
            Some(uuid),
            Some(Kind::Sell),
            Some(Status::Pending),
            100,
            "eur".to_string(),
            None,
            None,
            100,
            "SEPA".to_string(),
            1,
            None,
            None,
            None,
            Some(1627371434),
            None,
            None,
            None,
        ));

        let test_message = Message::Order(MessageKind::new(
            Some(uuid),
            Some(1),
            Some(2),
            Action::NewOrder,
            Some(payload),
        ));
        let test_message_json = test_message.as_json().unwrap();
        let sample_message = r#"{"order":{"version":1,"id":"308e1272-d5f4-47e6-bd97-3504baea9c23","request_id":1,"trade_index":2,"action":"new-order","payload":{"order":{"id":"308e1272-d5f4-47e6-bd97-3504baea9c23","kind":"sell","status":"pending","amount":100,"fiat_code":"eur","fiat_amount":100,"payment_method":"SEPA","premium":1,"created_at":1627371434}}}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let message_json = message.as_json().unwrap();
        assert_eq!(message_json, test_message_json);
    }

    #[test]
    fn test_payment_request_payload_message() {
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let test_message = Message::Order(MessageKind::new(
            Some(uuid),
            Some(1),
            Some(3),
            Action::PayInvoice,
            Some(Payload::PaymentRequest(
                Some(SmallOrder::new(
                    Some(uuid),
                    Some(Kind::Sell),
                    Some(Status::WaitingPayment),
                    100,
                    "eur".to_string(),
                    None,
                    None,
                    100,
                    "SEPA".to_string(),
                    1,
                    None,
                    None,
                    None,
                    Some(1627371434),
                    None,
                    None,
                    None,
                )),
                "lnbcrt78510n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e".to_string(),
                None,
            )),
        ));
        let sample_message = r#"{"order":{"version":1,"id":"308e1272-d5f4-47e6-bd97-3504baea9c23","request_id":1,"trade_index":3,"action":"pay-invoice","payload":{"payment_request":[{"id":"308e1272-d5f4-47e6-bd97-3504baea9c23","kind":"sell","status":"waiting-payment","amount":100,"fiat_code":"eur","fiat_amount":100,"payment_method":"SEPA","premium":1,"created_at":1627371434},"lnbcrt78510n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e",null]}}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let message_json = message.as_json().unwrap();
        let test_message_json = test_message.as_json().unwrap();
        assert_eq!(message_json, test_message_json);
    }

    #[test]
    fn test_message_payload_signature() {
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let peer = Peer::new(
            "npub1testjsf0runcqdht5apkfcalajxkf8txdxqqk5kgm0agc38ke4vsfsgzf8".to_string(),
        );
        let payload = Payload::Peer(peer);
        let test_message = Message::Order(MessageKind::new(
            Some(uuid),
            Some(1),
            Some(2),
            Action::FiatSentOk,
            Some(payload),
        ));
        assert!(test_message.verify());
        // Message should be signed with the trade keys
        let trade_keys =
            Keys::parse("110e43647eae221ab1da33ddc17fd6ff423f2b2f49d809b9ffa40794a2ab996c")
                .unwrap();
        let sig = test_message.get_inner_message_kind().sign(&trade_keys);

        assert!(test_message
            .get_inner_message_kind()
            .verify_signature(trade_keys.public_key(), sig));
    }

    #[test]
    fn test_cant_do_message_serialization() {
        // Test all CantDoReason variants
        let reasons = vec![
            CantDoReason::InvalidSignature,
            CantDoReason::InvalidTradeIndex,
            CantDoReason::InvalidAmount,
            CantDoReason::InvalidInvoice,
            CantDoReason::InvalidPaymentRequest,
            CantDoReason::InvalidPeer,
            CantDoReason::InvalidRating,
            CantDoReason::InvalidTextMessage,
            CantDoReason::InvalidOrderStatus,
            CantDoReason::InvalidPubkey,
            CantDoReason::InvalidParameters,
            CantDoReason::OrderAlreadyCanceled,
            CantDoReason::CantCreateUser,
        ];

        for reason in reasons {
            let cant_do = Message::CantDo(MessageKind::new(
                None,
                None,
                None,
                Action::CantDo,
                Some(Payload::CantDo(Some(reason.clone()))),
            ));
            let message = Message::from_json(&cant_do.as_json().unwrap()).unwrap();
            assert!(message.verify());
            assert_eq!(message.as_json().unwrap(), cant_do.as_json().unwrap());
        }

        // Test None case
        let cant_do = Message::CantDo(MessageKind::new(
            None,
            None,
            None,
            Action::CantDo,
            Some(Payload::CantDo(None)),
        ));
        let message = Message::from_json(&cant_do.as_json().unwrap()).unwrap();
        assert!(message.verify());
        assert_eq!(message.as_json().unwrap(), cant_do.as_json().unwrap());
    }
}
