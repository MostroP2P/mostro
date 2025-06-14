use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use serde_json::Value;

/// Extracts the LNURL from a given address.
/// The address can be in the form of a Lightning Address (user@domain.com format)
/// or a LNURL (lnurl1... format).
/// If the address is a Lightning Address, it is resolved to the corresponding LNURL.
/// If the address is already a LNURL, it is returned as is.
/// # Arguments
/// * `address` - The address to extract the LNURL from
/// # Returns
/// * `Ok(String)` - The extracted LNURL
/// * `Err(MostroError)` - If the address is invalid or cannot be resolved
async fn extract_lnurl(address: &str) -> Result<String, MostroError> {
    let url = if address.to_lowercase().starts_with("lnurl") {
        let lnurl = LnUrl::decode(address.to_string())
            .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
        lnurl.url
    } else {
        // Handle Lightning address format
        let (user, domain) = match address.split_once('@') {
            Some((user, domain)) => (user, domain),
            None => return Err(MostroInternalErr(ServiceError::LnAddressParseError)),
        };
        let base_url = if cfg!(test) {
            format!("http://{domain}:8080")
        } else {
            format!("https://{domain}")
        };
        format!("{base_url}/.well-known/lnurlp/{user}")
    };
    Ok(url)
}

pub async fn ln_exists(address: &str) -> Result<(), MostroError> {
    // Get the url from the str - could be a LNURL or a Lightning Address
    let url = extract_lnurl(address).await?;
    // Make the request to the LNURL
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
    // Get the url from the str - could be a LNURL or a Lightning Address
    let url = extract_lnurl(address).await?;
    // Convert the amount to msat
    let amount_msat = amount * 1000;

    // Make the request to the LNURL
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
