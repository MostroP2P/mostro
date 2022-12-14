use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Orders can be only Buy or Sell
#[derive(Debug, Deserialize, Serialize)]
pub enum Kind {
    Buy,
    Sell,
}

/// Each status that an order can have
#[derive(Debug, Deserialize, Serialize)]
pub enum Status {
    Active,
    Canceled,
    CanceledByAdmin,
    CompletedByAdmin,
    Dispute,
    Expired,
    FiatSent,
    SettledInvoice,
    Pending,
    Success,
    WaitingBuyerInvoice,
    WaitingPayment,
}

/// Action is used to identify each message between Mostro and users
#[derive(Debug, Deserialize, Serialize)]
pub enum Action {
    Order,
    PaymentRequest,
    FiatSent,
    Release,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Use this Message to establish communication between users and Mostro
#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    pub version: u8,
    pub action: Action,
    pub content: Option<Content>,
}

/// Message content
#[derive(Debug, Deserialize, Serialize)]
pub enum Content {
    Order(Order),
    PaymentRequest(String),
}

impl Message {
    /// New message from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }
    /// Get message as json string
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    /// Verify if is valid message
    pub fn verify(&self) -> bool {
        match &self.action {
            Action::Order => {
                if let Some(Content::Order(_)) = &self.content {
                    true
                } else {
                    false
                }
            }
            Action::PaymentRequest => {
                if let Some(Content::PaymentRequest(_)) = &self.content {
                    true
                } else {
                    false
                }
            }
            Action::FiatSent => true,
            Action::Release => true,
        }
    }
}

/// Mostro Order
#[derive(Debug, Deserialize, Serialize)]
pub struct Order {
    pub kind: Kind,
    pub status: Status,
    pub amount: u32,
    pub fiat_code: String,
    pub fiat_amount: u32,
    pub payment_method: String,
    pub prime: i8,
    pub payment_request: Option<String>,
    pub created_at: u64, // unix timestamp seconds
}

impl Order {
    /// New order from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Get order as json string
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }
}
