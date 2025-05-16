use mostro_core::prelude::*;
use serde_json::Value;

pub async fn ln_exists(address: &str) -> Result<(), MostroError> {
    let (user, domain) = match address.split_once('@') {
        Some((user, domain)) => (user, domain),
        None => return Err(MostroInternalErr(ServiceError::LnAddressParseError)),
    };

    let url = format!("https://{domain}/.well-known/lnurlp/{user}");
    let res = reqwest::get(url)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;
    let status = res.status();
    if status.is_success() {
        let body = res
            .text()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
        let tag = body["tag"].as_str().unwrap_or("");
        if tag == "payRequest" {
            return Ok(());
        }
        Err(MostroInternalErr(ServiceError::LnAddressParseError))
    } else {
        Err(MostroInternalErr(ServiceError::LnAddressParseError))
    }
}

pub async fn resolv_ln_address(address: &str, amount: u64) -> Result<String, MostroError> {
    let (user, domain) = match address.split_once('@') {
        Some((user, domain)) => (user, domain),
        None => return Ok("".to_string()),
    };
    let amount_msat = amount * 1000;

    let url = format!("https://{domain}/.well-known/lnurlp/{user}");
    let res = reqwest::get(url)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
    let status = res.status();
    if status.is_success() {
        let body = res
            .text()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
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
            .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
        let status = res.status();
        if status.is_success() {
            let body = res
                .text()
                .await
                .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
            let body: Value = serde_json::from_str(&body)
                .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
            let pr = body["pr"].as_str().unwrap_or("");

            return Ok(pr.to_string());
        }
        Ok("".to_string())
    } else {
        Ok("".to_string())
    }
}
