use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Yadio {
    request: Request,
    pub result: f64,
    rate: f64,
    timestamp: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    amount: i64,
    from: String,
    to: String,
}
