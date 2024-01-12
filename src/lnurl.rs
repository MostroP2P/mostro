use anyhow::{Context, Result};
use serde_json::Value;

pub async fn ln_exists(address: &str) -> Result<bool> {
    let (user, domain) = match address.split_once('@') {
        Some((user, domain)) => (user, domain),
        None => return Ok(false),
    };

    let url = format!("https://{domain}/.well-known/lnurlp/{user}");
    let res = reqwest::get(url)
        .await
        .context("Something went wrong with API request, try again!")?;
    let status = res.status();
    if status.is_success() {
        let body = res.text().await?;
        let body: Value = serde_json::from_str(&body)?;
        let tag = body["tag"].as_str().unwrap_or("");
        if tag == "payRequest" {
            return Ok(true);
        }
        Ok(false)
    } else {
        Ok(false)
    }
}

pub async fn resolv_ln_address(address: &str, amount: u64) -> Result<String> {
    let (user, domain) = match address.split_once('@') {
        Some((user, domain)) => (user, domain),
        None => return Ok("".to_string()),
    };
    let amount_msat = amount * 1000;

    let url = format!("https://{domain}/.well-known/lnurlp/{user}");
    let res = reqwest::get(url)
        .await
        .context("Something went wrong with API request, try again!")?;
    let status = res.status();
    if status.is_success() {
        let body = res.text().await?;
        let body: Value = serde_json::from_str(&body)?;
        let tag = body["tag"].as_str().unwrap_or("");
        if tag != "payRequest" {
            return Ok("".to_string());
        }
        let min = body["minSendable"].as_u64().unwrap_or(0);
        let max = body["maxSendable"].as_u64().unwrap_or(0);
        if min > amount_msat || max < amount_msat {
            return Ok("".to_string());
        }
        let callback = body["callback"].as_str().unwrap_or("");
        let callback = format!("{callback}?amount={amount_msat}");
        let res = reqwest::get(callback)
            .await
            .context("Something went wrong with API request, try again!")?;
        let status = res.status();
        if status.is_success() {
            let body = res.text().await?;
            let body: Value = serde_json::from_str(&body)?;
            let pr = body["pr"].as_str().unwrap_or("");

            return Ok(pr.to_string());
        }
        Ok("".to_string())
    } else {
        Ok("".to_string())
    }
}
