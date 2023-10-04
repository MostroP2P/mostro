use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nostr::event::builder::Error;
use nostr_sdk::prelude::*;

pub fn new_event(
    keys: &Keys,
    content: String,
    d_str: String,
    k_str: Option<String>,
    f_str: Option<String>,
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

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content, &tags).to_event(keys)
}
