use anyhow::{Ok, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Orders can be only Buy or Sell
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Buy,
    Sell,
}

impl FromStr for Kind {
    type Err = ();

    fn from_str(kind: &str) -> std::result::Result<Kind, Self::Err> {
        match kind {
            "Buy" => std::result::Result::Ok(Kind::Buy),
            "Sell" => std::result::Result::Ok(Kind::Sell),
            _ => Err(()),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Each status that an order can have
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum Status {
    Active,
    Canceled,
    CanceledByAdmin,
    CompletedByAdmin,
    Dispute,
    Expired,
    FiatSent,
    SettledHoldInvoice,
    Pending,
    Success,
    WaitingBuyerInvoice,
    WaitingPayment,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Action is used to identify each message between Mostro and users
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Action {
    Order,
    TakeSell,
    TakeBuy,
    PayInvoice,
    FiatSent,
    Release,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Use this Message to establish communication between users and Mostro
#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    pub version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<i64>,
    pub action: Action,
    pub content: Option<Content>,
}

/// Message content
#[derive(Debug, Deserialize, Serialize)]
pub enum Content {
    Order(Order),
    PaymentRequest(String),
}

#[allow(dead_code)]
impl Message {
    /// New message from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }
    /// Get message as json string
    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    /// Verify if is valid message
    pub fn verify(&self) -> bool {
        match &self.action {
            Action::Order => matches!(&self.content, Some(Content::Order(_))),
            Action::TakeSell => {
                if self.order_id.is_none() {
                    return false;
                }
                matches!(&self.content, Some(Content::PaymentRequest(_)))
            }
            Action::TakeBuy => {
                todo!()
            }
            Action::PayInvoice => {
                todo!()
            }
            Action::FiatSent => {
                if self.order_id.is_none() {
                    return false;
                }
                true
            }
            Action::Release => {
                if self.order_id.is_none() {
                    return false;
                }
                true
            }
        }
    }

    pub fn get_order(&self) -> Option<&Order> {
        if self.action != Action::Order {
            return None;
        }
        match &self.content {
            Some(Content::Order(o)) => Some(o),
            _ => None,
        }
    }

    pub fn get_payment_request(&self) -> Option<String> {
        if self.action != Action::TakeSell {
            return None;
        }
        match &self.content {
            Some(Content::PaymentRequest(pr)) => Some(pr.to_owned()),
            _ => None,
        }
    }
}

/// Mostro Order
#[derive(Debug, Deserialize, Serialize)]
pub struct Order {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub kind: Kind,
    pub status: Status,
    pub amount: u32,
    pub fiat_code: String,
    pub fiat_amount: u32,
    pub payment_method: String,
    pub prime: i8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>, // unix timestamp seconds
}

#[allow(dead_code)]
impl Order {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Option<i64>,
        kind: Kind,
        status: Status,
        amount: u32,
        fiat_code: String,
        fiat_amount: u32,
        payment_method: String,
        prime: i8,
        payment_request: Option<String>,
        created_at: Option<u64>,
    ) -> Self {
        Self {
            id,
            kind,
            status,
            amount,
            fiat_code,
            fiat_amount,
            payment_method,
            prime,
            payment_request,
            created_at,
        }
    }
    /// New order from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Get order as json string
    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }
}
