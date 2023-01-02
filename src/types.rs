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
        write!(f, "{:?}", self)
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
    SettledInvoice,
    Pending,
    Success,
    WaitingBuyerInvoice,
    WaitingPayment,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Action is used to identify each message between Mostro and users
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Action {
    Order,
    PaymentRequest,
    FiatSent,
    Release,
    ListOffers,
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
    OrderStatus(Status),
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
            Action::PaymentRequest => matches!(&self.content, Some(Content::PaymentRequest(_))),
            Action::FiatSent => true,
            Action::Release => true,
            Action::ListOffers => true,
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
        if self.action != Action::PaymentRequest {
            return None;
        }
        match &self.content {
            Some(Content::PaymentRequest(pr)) => Some(pr.to_owned()),
            _ => None,
        }
    }

    pub fn get_order_list_status(&self) -> Option<Status> {
        if self.action != Action::ListOffers {
            return None;
        }
        match &self.content {
            Some(Content::OrderStatus(ord)) => Some(ord.to_owned()),
            _ => Some(Status::Pending),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>, // unix timestamp seconds
}

#[allow(dead_code)]
impl Order {
    pub fn new(
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
