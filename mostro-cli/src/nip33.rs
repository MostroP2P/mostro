use anyhow::{Ok, Result};
use mostro_core::dispute::{Dispute, Status as DisputeStatus};
use mostro_core::order::{Kind as OrderKind, SmallOrder, Status};
use nostr_sdk::prelude::*;
use std::str::FromStr;
use uuid::Uuid;

pub fn order_from_tags(tags: Tags) -> Result<SmallOrder> {
    let mut order = SmallOrder::default();
    for tag in tags {
        let t = tag.to_vec();
        let v = t.get(1).unwrap().as_str();
        match t.first().unwrap().as_str() {
            "d" => {
                let id = v.parse::<Uuid>();
                let id = match id {
                    core::result::Result::Ok(id) => Some(id),
                    Err(_) => None,
                };
                order.id = id;
            }
            "k" => {
                order.kind = Some(OrderKind::from_str(v).unwrap());
            }
            "f" => {
                order.fiat_code = v.to_string();
            }
            "s" => {
                order.status = Some(Status::from_str(v).unwrap_or(Status::Dispute));
            }
            "amt" => {
                order.amount = v.parse::<i64>().unwrap();
            }
            "fa" => {
                if v.contains('.') {
                    continue;
                }
                let max = t.get(2);
                if max.is_some() {
                    order.min_amount = v.parse::<i64>().ok();
                    order.max_amount = max.unwrap().parse::<i64>().ok();
                } else {
                    let fa = v.parse::<i64>();
                    order.fiat_amount = fa.unwrap_or(0);
                }
            }
            "pm" => {
                order.payment_method = v.to_string();
            }
            "premium" => {
                order.premium = v.parse::<i64>().unwrap();
            }
            _ => {}
        }
    }

    Ok(order)
}

pub fn dispute_from_tags(tags: Tags) -> Result<Dispute> {
    let mut dispute = Dispute::default();
    for tag in tags {
        let t = tag.to_vec();
        let v = t.get(1).unwrap().as_str();
        match t.first().unwrap().as_str() {
            "d" => {
                let id = t.get(1).unwrap().as_str().parse::<Uuid>();
                let id = match id {
                    core::result::Result::Ok(id) => id,
                    Err(_) => return Err(anyhow::anyhow!("Invalid dispute id")),
                };
                dispute.id = id;
            }

            "s" => {
                let status = match DisputeStatus::from_str(v) {
                    core::result::Result::Ok(status) => status,
                    Err(_) => return Err(anyhow::anyhow!("Invalid dispute status")),
                };

                dispute.status = status.to_string();
            }

            _ => {}
        }
    }

    Ok(dispute)
}
