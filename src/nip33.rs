use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nostr::event::builder::Error;
use nostr_sdk::prelude::*;

/// Creates a new mostro nip33 event
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `d_str` - The nip33 d tag used to replaced the event with a new one
/// * `k_str` - The nip33 k tag used to subscribe to sell/buy order type notifications
/// * `f_str` - The nip33 f tag used to subscribe to fiat currency code type notifications
/// * `s_str` - The nip33 s tag used to subscribe to order status type notifications
pub fn new_event(
    keys: &Keys,
    content: String,
    d_str: String,
    k_str: Option<String>,
    f_str: Option<String>,
    s_str: Option<String>,
) -> Result<Event, Error> {
    // This tag (nip33) allows us to change this event in particular in the future
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![d_str]);
    let mut tags = vec![d_tag];
    if let Some(k) = k_str {
        // This tag helps client to subscribe to sell/buy order type notifications
        let k_tag = Tag::Generic(TagKind::Custom("k".to_string()), vec![k]);
        tags.push(k_tag);
    }

    if let Some(f) = f_str {
        // This tag helps client to subscribe to fiat(shit) coin name
        let f_tag = Tag::Generic(TagKind::Custom("f".to_string()), vec![f]);
        tags.push(f_tag);
    }

    if let Some(s) = s_str {
        // This tag helps client to subscribe to order status
        let s_tag = Tag::Generic(TagKind::Custom("s".to_string()), vec![s]);
        tags.push(s_tag);
    }

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content, &tags).to_event(keys)
}
