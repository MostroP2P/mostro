use crate::app::context::AppContext;
use crate::db::add_new_user;
use crate::util::send_dm;
use mostro_core::prelude::*;
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use tracing::{error, info};

pub const SOLVER_CATEGORY_READ_ONLY: i64 = 1;
pub const SOLVER_CATEGORY_READ_WRITE: i64 = 2;

fn parse_solver_payload(payload: &Payload) -> Result<(String, i64), MostroError> {
    let raw = match payload {
        Payload::TextMessage(p) => p.trim(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidTextMessage)),
    };

    if raw.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidTextMessage));
    }

    let mut parts = raw.split(':');
    let npub = parts
        .next()
        .ok_or(MostroCantDo(CantDoReason::InvalidTextMessage))?
        .trim();

    if npub.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidTextMessage));
    }

    let category = match parts.next().map(str::trim) {
        None => SOLVER_CATEGORY_READ_WRITE,
        Some("read") => SOLVER_CATEGORY_READ_ONLY,
        Some("read-write") | Some("write") => SOLVER_CATEGORY_READ_WRITE,
        Some("") | Some(_) => return Err(MostroCantDo(CantDoReason::InvalidParameters)),
    };

    if parts.next().is_some() {
        return Err(MostroCantDo(CantDoReason::InvalidParameters));
    }

    Ok((npub.to_string(), category))
}

pub async fn admin_add_solver_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let request_id = msg.get_inner_message_kind().request_id;

    let inner_message = msg.get_inner_message_kind();
    let payload = inner_message
        .payload
        .as_ref()
        .ok_or(MostroCantDo(CantDoReason::InvalidTextMessage))?;

    if event.identity != my_keys.public_key() {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    }

    let trade_index = inner_message.trade_index.unwrap_or(0);
    let (npubkey, category) = parse_solver_payload(payload)?;
    let public_key = PublicKey::from_bech32(&npubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    let user = User::new(public_key.to_string(), 0, 1, 0, category, trade_index);

    match add_new_user(pool, user).await {
        Ok(r) => info!("Solver added: {} with category {}", r, category),
        Err(ee) => {
            error!("Error creating solver: {:#?}", ee);
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                ee.to_string(),
            )));
        }
    }

    let message = Message::new_dispute(None, request_id, None, Action::AdminAddSolver, None);
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;

    send_dm(event.sender, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_solver_payload, SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE};
    use mostro_core::error::CantDoReason;
    use mostro_core::message::Payload;

    #[test]
    fn parse_solver_payload_defaults_to_read_write() {
        let (npub, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test".to_string())).unwrap();
        assert_eq!(npub, "npub1test");
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);
    }

    #[test]
    fn parse_solver_payload_accepts_read_only() {
        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:read".to_string())).unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_ONLY);
    }

    #[test]
    fn parse_solver_payload_accepts_read_write_aliases() {
        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:read-write".to_string()))
                .unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);

        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:write".to_string())).unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);
    }

    #[test]
    fn parse_solver_payload_rejects_invalid_permission() {
        let err =
            parse_solver_payload(&Payload::TextMessage("npub1test:admin".to_string())).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidParameters)
        );
    }

    #[test]
    fn parse_solver_payload_rejects_empty_permission_token() {
        let err =
            parse_solver_payload(&Payload::TextMessage("npub1test:".to_string())).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidParameters)
        );
    }
}
