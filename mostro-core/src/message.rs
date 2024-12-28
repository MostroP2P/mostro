use crate::order::SmallOrder;
use crate::PROTOCOL_VER;
use anyhow::{Ok, Result};
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::hashes::Hash;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::Message as BitcoinMessage;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// One party of the trade
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Peer {
    pub pubkey: String,
}

impl Peer {
    pub fn new(pubkey: String) -> Self {
        Self { pubkey }
    }

    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }
}

/// Action is used to identify each message between Mostro and users
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    NewOrder,
    TakeSell,
    TakeBuy,
    PayInvoice,
    FiatSent,
    FiatSentOk,
    Release,
    Released,
    Cancel,
    Canceled,
    CooperativeCancelInitiatedByYou,
    CooperativeCancelInitiatedByPeer,
    DisputeInitiatedByYou,
    DisputeInitiatedByPeer,
    CooperativeCancelAccepted,
    BuyerInvoiceAccepted,
    PurchaseCompleted,
    HoldInvoicePaymentAccepted,
    HoldInvoicePaymentSettled,
    HoldInvoicePaymentCanceled,
    WaitingSellerToPay,
    WaitingBuyerInvoice,
    AddInvoice,
    BuyerTookOrder,
    Rate,
    RateUser,
    RateReceived,
    CantDo,
    Dispute,
    AdminCancel,
    AdminCanceled,
    AdminSettle,
    AdminSettled,
    AdminAddSolver,
    AdminTakeDispute,
    AdminTookDispute,
    IsNotYourOrder,
    NotAllowedByStatus,
    OutOfRangeFiatAmount,
    IsNotYourDispute,
    NotFound,
    IncorrectInvoiceAmount,
    InvalidSatsAmount,
    OutOfRangeSatsAmount,
    PaymentFailed,
    InvoiceUpdated,
    SendDm,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Use this Message to establish communication between users and Mostro
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Message {
    Order(MessageKind),
    Dispute(MessageKind),
    CantDo(MessageKind),
    Rate(MessageKind),
    Dm(MessageKind),
}

impl Message {
    /// New order message
    pub fn new_order(
        id: Option<Uuid>,
        request_id: Option<u64>,
        trade_index: Option<i64>,
        action: Action,
        payload: Option<Payload>,
    ) -> Self {
        let kind = MessageKind::new(id, request_id, trade_index, action, payload);
        Self::Order(kind)
    }

    /// New dispute message
    pub fn new_dispute(
        id: Option<Uuid>,
        request_id: Option<u64>,
        trade_index: Option<i64>,
        action: Action,
        payload: Option<Payload>,
    ) -> Self {
        let kind = MessageKind::new(id, request_id, trade_index, action, payload);

        Self::Dispute(kind)
    }

    /// New can't do template message message
    pub fn cant_do(id: Option<Uuid>, request_id: Option<u64>, payload: Option<Payload>) -> Self {
        let kind = MessageKind::new(id, request_id, None, Action::CantDo, payload);

        Self::CantDo(kind)
    }

    /// New DM message
    pub fn new_dm(
        id: Option<Uuid>,
        request_id: Option<u64>,
        action: Action,
        payload: Option<Payload>,
    ) -> Self {
        let kind = MessageKind::new(id, request_id, None, action, payload);

        Self::Dm(kind)
    }

    /// Get message from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Get message as json string
    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    // Get inner message kind
    pub fn get_inner_message_kind(&self) -> &MessageKind {
        match self {
            Message::Dispute(k)
            | Message::Order(k)
            | Message::CantDo(k)
            | Message::Rate(k)
            | Message::Dm(k) => k,
        }
    }

    // Get action from the inner message
    pub fn inner_action(&self) -> Option<Action> {
        match self {
            Message::Dispute(a)
            | Message::Order(a)
            | Message::CantDo(a)
            | Message::Rate(a)
            | Message::Dm(a) => Some(a.get_action()),
        }
    }

    /// Verify if is valid the inner message
    pub fn verify(&self) -> bool {
        match self {
            Message::Order(m)
            | Message::Dispute(m)
            | Message::CantDo(m)
            | Message::Rate(m)
            | Message::Dm(m) => m.verify(),
        }
    }
}

/// Use this Message to establish communication between users and Mostro
#[derive(Debug, Deserialize, Serialize)]
pub struct MessageKind {
    /// Message version
    pub version: u8,
    /// Request_id for test on client
    pub request_id: Option<u64>,
    /// Trade key index
    pub trade_index: Option<i64>,
    /// Message id is not mandatory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Action to be taken
    pub action: Action,
    /// Payload of the Message
    pub payload: Option<Payload>,
}

type Amount = i64;

/// Represents specific reasons why a requested action cannot be performed
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CantDoReason {
    /// The provided signature is invalid or missing
    InvalidSignature,
    /// The specified trade index does not exist or is invalid
    InvalidTradeIndex,
    /// The provided amount is invalid or out of acceptable range
    InvalidAmount,
    /// The provided invoice is malformed or expired
    InvalidInvoice,
    /// The payment request is invalid or cannot be processed
    InvalidPaymentRequest,
    /// The specified peer is invalid or not found
    InvalidPeer,
    /// The rating value is invalid or out of range
    InvalidRating,
    /// The text message is invalid or contains prohibited content
    InvalidTextMessage,
    /// The order kind is invalid
    InvalidOrderKind,
    /// The order status is invalid
    InvalidOrderStatus,
    /// Invalid pubkey
    InvalidPubkey,
    /// Invalid parameters
    InvalidParameters,
    /// The order is already canceled
    OrderAlreadyCanceled,
    /// Can't create user
    CantCreateUser,
}

/// Message payload
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Order(SmallOrder),
    PaymentRequest(Option<SmallOrder>, String, Option<Amount>),
    TextMessage(String),
    Peer(Peer),
    RatingUser(u8),
    Amount(Amount),
    Dispute(Uuid, Option<u16>),
    CantDo(Option<CantDoReason>),
}

#[allow(dead_code)]
impl MessageKind {
    /// New message
    pub fn new(
        id: Option<Uuid>,
        request_id: Option<u64>,
        trade_index: Option<i64>,
        action: Action,
        payload: Option<Payload>,
    ) -> Self {
        Self {
            version: PROTOCOL_VER,
            request_id,
            trade_index,
            id,
            action,
            payload,
        }
    }
    /// Get message from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }
    /// Get message as json string
    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    // Get action from the inner message
    pub fn get_action(&self) -> Action {
        self.action.clone()
    }

    /// Verify if is valid message
    pub fn verify(&self) -> bool {
        match &self.action {
            Action::NewOrder => matches!(&self.payload, Some(Payload::Order(_))),
            Action::PayInvoice | Action::AddInvoice => {
                if self.id.is_none() {
                    return false;
                }
                matches!(&self.payload, Some(Payload::PaymentRequest(_, _, _)))
            }
            Action::TakeSell
            | Action::TakeBuy
            | Action::FiatSent
            | Action::FiatSentOk
            | Action::Release
            | Action::Released
            | Action::Dispute
            | Action::AdminCancel
            | Action::AdminCanceled
            | Action::AdminSettle
            | Action::AdminSettled
            | Action::Rate
            | Action::RateReceived
            | Action::AdminTakeDispute
            | Action::AdminTookDispute
            | Action::DisputeInitiatedByYou
            | Action::DisputeInitiatedByPeer
            | Action::WaitingBuyerInvoice
            | Action::PurchaseCompleted
            | Action::HoldInvoicePaymentAccepted
            | Action::HoldInvoicePaymentSettled
            | Action::HoldInvoicePaymentCanceled
            | Action::WaitingSellerToPay
            | Action::BuyerTookOrder
            | Action::BuyerInvoiceAccepted
            | Action::CooperativeCancelInitiatedByYou
            | Action::CooperativeCancelInitiatedByPeer
            | Action::CooperativeCancelAccepted
            | Action::Cancel
            | Action::IsNotYourOrder
            | Action::NotAllowedByStatus
            | Action::OutOfRangeFiatAmount
            | Action::OutOfRangeSatsAmount
            | Action::IsNotYourDispute
            | Action::NotFound
            | Action::IncorrectInvoiceAmount
            | Action::InvalidSatsAmount
            | Action::PaymentFailed
            | Action::InvoiceUpdated
            | Action::AdminAddSolver
            | Action::SendDm
            | Action::Canceled => {
                if self.id.is_none() {
                    return false;
                }
                true
            }
            Action::RateUser => {
                matches!(&self.payload, Some(Payload::RatingUser(_)))
            }
            Action::CantDo => {
                matches!(&self.payload, Some(Payload::CantDo(_)))
            }
        }
    }

    pub fn get_order(&self) -> Option<&SmallOrder> {
        if self.action != Action::NewOrder {
            return None;
        }
        match &self.payload {
            Some(Payload::Order(o)) => Some(o),
            _ => None,
        }
    }

    pub fn get_payment_request(&self) -> Option<String> {
        if self.action != Action::TakeSell
            && self.action != Action::AddInvoice
            && self.action != Action::NewOrder
        {
            return None;
        }
        match &self.payload {
            Some(Payload::PaymentRequest(_, pr, _)) => Some(pr.to_owned()),
            Some(Payload::Order(ord)) => ord.buyer_invoice.to_owned(),
            _ => None,
        }
    }

    pub fn get_amount(&self) -> Option<Amount> {
        if self.action != Action::TakeSell && self.action != Action::TakeBuy {
            return None;
        }
        match &self.payload {
            Some(Payload::PaymentRequest(_, _, amount)) => *amount,
            Some(Payload::Amount(amount)) => Some(*amount),
            _ => None,
        }
    }

    pub fn get_payload(&self) -> Option<&Payload> {
        self.payload.as_ref()
    }

    pub fn has_trade_index(&self) -> (bool, i64) {
        if let Some(index) = self.trade_index {
            return (true, index);
        }
        (false, 0)
    }

    pub fn sign(&self, keys: &Keys) -> Signature {
        let message = self.as_json().unwrap();
        let hash: Sha256Hash = Sha256Hash::hash(message.as_bytes());
        let hash = hash.to_byte_array();
        let message: BitcoinMessage = BitcoinMessage::from_digest(hash);

        keys.sign_schnorr(&message)
    }

    pub fn verify_signature(&self, pubkey: PublicKey, sig: Signature) -> bool {
        // Create message hash
        let message = self.as_json().unwrap();
        let hash: Sha256Hash = Sha256Hash::hash(message.as_bytes());
        let hash = hash.to_byte_array();
        let message: BitcoinMessage = BitcoinMessage::from_digest(hash);
        // Create a verification-only context for better performance
        let secp = Secp256k1::verification_only();
        // Verify signature
        pubkey.verify(&secp, &message, &sig).is_ok()
    }
}
