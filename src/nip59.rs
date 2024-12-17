use base64::engine::{general_purpose, Engine};
use mostro_core::message::Message;
use nostr_sdk::event::builder::Error as BuilderError;
use nostr_sdk::nostr::nips::nip44::v2::{decrypt_to_bytes, encrypt_to_bytes, ConversationKey};
use nostr_sdk::prelude::*;

/// Creates a new nip59 event
///
/// # Arguments
///
/// * `sender_keys` - The keys of the sender
/// * `receiver` - The public key of the receiver
/// * `rumor` - A regular nostr event, but is not signed.
/// * `expiration` - Time of the expiration of the event
///
/// # Returns
/// Returns a gift wrap event
///
pub fn gift_wrap(
    sender_keys: &Keys,
    receiver: PublicKey,
    payload: String,
    expiration: Option<Timestamp>,
) -> Result<Event, BuilderError> {
    // We convert back the string to a message
    let message = Message::from_json(&payload).unwrap();
    // We sign the message
    let sig = message.get_inner_message_kind().sign(sender_keys);
    // We compose the content
    let content = (message, sig);
    let content = serde_json::to_string(&content).unwrap();
    let rumor: UnsignedEvent = EventBuilder::text_note(content).build(sender_keys.public_key());
    let seal: Event = seal(sender_keys, &receiver, rumor)?.sign_with_keys(sender_keys)?;

    gift_wrap_from_seal(&receiver, &seal, expiration)
}

pub fn seal(
    sender_keys: &Keys,
    receiver_pubkey: &PublicKey,
    rumor: UnsignedEvent,
) -> Result<EventBuilder, BuilderError> {
    // Derive conversation key
    let ck = ConversationKey::derive(sender_keys.secret_key(), receiver_pubkey);
    // Encrypt content
    let encrypted_content = encrypt_to_bytes(&ck, rumor.as_json())?;
    // Encode with base64
    let b64decoded_content = general_purpose::STANDARD.encode(encrypted_content);
    // Compose builder
    Ok(EventBuilder::new(Kind::Seal, b64decoded_content)
        .custom_created_at(Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK)))
}

pub fn gift_wrap_from_seal(
    receiver: &PublicKey,
    seal: &Event,
    expiration: Option<Timestamp>,
) -> Result<Event, BuilderError> {
    let ephemeral_keys: Keys = Keys::generate();
    // Derive conversation key
    let ck = ConversationKey::derive(ephemeral_keys.secret_key(), receiver);
    // Encrypt content
    let encrypted_content = encrypt_to_bytes(&ck, seal.as_json())?;

    let mut tags: Vec<Tag> = Vec::with_capacity(1 + usize::from(expiration.is_some()));
    tags.push(Tag::public_key(*receiver));

    if let Some(timestamp) = expiration {
        tags.push(Tag::expiration(timestamp));
    }
    let tags = Tags::new(tags);
    // Encode with base64
    let b64decoded_content = general_purpose::STANDARD.encode(encrypted_content);
    let event = EventBuilder::new(Kind::GiftWrap, b64decoded_content)
        .tags(tags)
        .custom_created_at(Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK))
        .build(ephemeral_keys.public_key())
        .sign_with_keys(&ephemeral_keys)?;

    Ok(event)
}

pub fn unwrap_gift_wrap(keys: &Keys, gift_wrap: &Event) -> Result<UnwrappedGift, BuilderError> {
    let ck = ConversationKey::derive(keys.secret_key(), &gift_wrap.pubkey);
    let b64decoded_content = match general_purpose::STANDARD.decode(gift_wrap.content.as_bytes()) {
        Ok(b64decoded_content) => b64decoded_content,
        Err(e) => return Err(BuilderError::NIP44(nip44::Error::NotFound(e.to_string()))),
    };
    // Decrypt and verify seal
    let seal = decrypt_to_bytes(&ck, b64decoded_content)?;
    let seal = String::from_utf8(seal).expect("Found invalid UTF-8");
    let seal = match Event::from_json(seal) {
        Ok(seal) => seal,
        Err(e) => return Err(BuilderError::NIP44(nip44::Error::NotFound(e.to_string()))),
    };

    let ck = ConversationKey::derive(keys.secret_key(), &seal.pubkey);
    let b64decoded_content = match general_purpose::STANDARD.decode(seal.content.as_bytes()) {
        Ok(b64decoded_content) => b64decoded_content,
        Err(e) => return Err(BuilderError::NIP44(nip44::Error::NotFound(e.to_string()))),
    };
    // Decrypt rumor
    let rumor = decrypt_to_bytes(&ck, b64decoded_content)?;
    let rumor = String::from_utf8(rumor).expect("Found invalid UTF-8");

    Ok(UnwrappedGift {
        sender: seal.pubkey,
        rumor: UnsignedEvent::from_json(rumor)?,
    })
}
