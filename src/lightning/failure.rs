//! Why an outgoing payment failed, in one log line.
//!
//! LND's `Payment` already says why a payment failed (`failure_reason`) and,
//! per HTLC attempt, which hop returned which BOLT #4 failure. That is what
//! tells an operator a problem on their own node (liquidity, fee cap) from
//! one on the payee's side (an offline wallet, an LSP that cannot forward).
//! Only pubkeys and codes are kept: never the invoice, the preimage or who
//! the payee is in Mostro.

use fedimint_tonic_lnd::lnrpc::failure::FailureCode;
use fedimint_tonic_lnd::lnrpc::htlc_attempt::HtlcStatus;
use fedimint_tonic_lnd::lnrpc::{HtlcAttempt, Payment, PaymentFailureReason};
use std::fmt;

/// The failure a failed attempt reported, and where on its route.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptFailure {
    code: String,
    /// Position of the node that returned the failure: 0 is our own node,
    /// `n` is the `n`-th hop of the route.
    hop_index: u32,
    route_hops: usize,
    /// The failing node's pubkey; `None` for our node or an index past the
    /// end of the route.
    node: Option<String>,
}

/// Why a payment failed, built from LND's terminal `Payment` update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutFailure {
    reason: String,
    attempts: usize,
    last_failure: Option<AttemptFailure>,
    amount_sat: i64,
    fee_cap_sat: i64,
}

impl PayoutFailure {
    /// `fee_cap_sat` is the routing fee limit the payment was sent with
    /// (`routing_fee_cap_sats`); LND does not echo it back.
    pub fn from_payment(payment: &Payment, fee_cap_sat: i64) -> Self {
        let reason = PaymentFailureReason::try_from(payment.failure_reason)
            .map(|reason| reason.as_str_name().to_string())
            .unwrap_or_else(|_| format!("UNKNOWN({})", payment.failure_reason));
        Self {
            reason,
            attempts: payment.htlcs.len(),
            last_failure: payment.htlcs.iter().rev().find_map(attempt_failure),
            amount_sat: payment.value_sat,
            fee_cap_sat,
        }
    }
}

fn attempt_failure(attempt: &HtlcAttempt) -> Option<AttemptFailure> {
    if attempt.status != HtlcStatus::Failed as i32 {
        return None;
    }
    let failure = attempt.failure.as_ref()?;
    let hops = attempt
        .route
        .as_ref()
        .map(|route| route.hops.as_slice())
        .unwrap_or_default();
    let hop_index = failure.failure_source_index;
    // LND's route lists the hops after our node, so index `n` (n >= 1) is
    // `hops[n - 1]`; index 0 is our node, which has no hop entry.
    let node = usize::try_from(hop_index)
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| hops.get(index))
        .map(|hop| hop.pub_key.clone());
    let code = FailureCode::try_from(failure.code)
        .map(|code| code.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", failure.code));
    Some(AttemptFailure {
        code,
        hop_index,
        route_hops: hops.len(),
        node,
    })
}

impl fmt::Display for PayoutFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reason={} attempts={}", self.reason, self.attempts)?;
        match &self.last_failure {
            Some(last) => {
                write!(
                    f,
                    " last_failure={} at hop {} of {} ",
                    last.code, last.hop_index, last.route_hops
                )?;
                match (&last.node, last.hop_index) {
                    (Some(node), _) => write!(f, "(node {node})")?,
                    (None, 0) => write!(f, "(our node)")?,
                    (None, _) => write!(f, "(outside the route)")?,
                }
            }
            None if self.attempts == 0 => write!(f, " (no HTLC was sent)")?,
            None => write!(f, " (no attempt reported a failure)")?,
        }
        write!(
            f,
            " amount_sat={} fee_cap_sat={}",
            self.amount_sat, self.fee_cap_sat
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimint_tonic_lnd::lnrpc::failure::FailureCode;
    use fedimint_tonic_lnd::lnrpc::htlc_attempt::HtlcStatus;
    use fedimint_tonic_lnd::lnrpc::{
        payment, Failure, Hop, HtlcAttempt, Payment, PaymentFailureReason, Route,
    };

    const PEER: &str = "02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LSP: &str = "02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PAYEE: &str = "03cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const INVOICE: &str = "lnbc643u1secretinvoice";
    const PREIMAGE: &str = "ffeeddccbbaa";

    /// A route as LND reports it: the hops after our node, ours excluded.
    fn route(pubkeys: &[&str]) -> Route {
        Route {
            hops: pubkeys
                .iter()
                .map(|pub_key| Hop {
                    pub_key: pub_key.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn failed_attempt(pubkeys: &[&str], code: FailureCode, source_index: u32) -> HtlcAttempt {
        HtlcAttempt {
            status: HtlcStatus::Failed as i32,
            route: Some(route(pubkeys)),
            failure: Some(Failure {
                code: code as i32,
                failure_source_index: source_index,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn failed_payment(reason: PaymentFailureReason, htlcs: Vec<HtlcAttempt>) -> Payment {
        Payment {
            payment_hash: "80d4b651".to_string(),
            value_sat: 64_287,
            status: payment::PaymentStatus::Failed as i32,
            failure_reason: reason as i32,
            payment_request: INVOICE.to_string(),
            payment_preimage: PREIMAGE.to_string(),
            htlcs,
            ..Default::default()
        }
    }

    #[test]
    fn a_failure_names_the_reason_the_attempts_and_the_hop_that_failed_last() {
        let payment = failed_payment(
            PaymentFailureReason::FailureReasonError,
            vec![
                failed_attempt(&[PEER, PAYEE], FailureCode::TemporaryChannelFailure, 1),
                failed_attempt(&[PEER, LSP, PAYEE], FailureCode::UnknownNextPeer, 2),
            ],
        );

        let failure = PayoutFailure::from_payment(&payment, 128);

        assert_eq!(
            failure.to_string(),
            format!(
                "reason=FAILURE_REASON_ERROR attempts=2 \
                 last_failure=UNKNOWN_NEXT_PEER at hop 2 of 3 (node {LSP}) \
                 amount_sat=64287 fee_cap_sat=128"
            )
        );
    }

    #[test]
    fn a_failure_with_no_htlc_attempt_says_none_was_sent() {
        let payment = failed_payment(PaymentFailureReason::FailureReasonNoRoute, vec![]);

        let failure = PayoutFailure::from_payment(&payment, 128);

        assert_eq!(
            failure.to_string(),
            "reason=FAILURE_REASON_NO_ROUTE attempts=0 (no HTLC was sent) \
             amount_sat=64287 fee_cap_sat=128"
        );
    }

    #[test]
    fn a_failure_returned_by_our_own_node_is_named_as_ours() {
        let payment = failed_payment(
            PaymentFailureReason::FailureReasonInsufficientBalance,
            vec![failed_attempt(
                &[PEER, PAYEE],
                FailureCode::TemporaryChannelFailure,
                0,
            )],
        );

        let text = PayoutFailure::from_payment(&payment, 128).to_string();

        assert!(
            text.contains("last_failure=TEMPORARY_CHANNEL_FAILURE at hop 0 of 2 (our node)"),
            "{text}"
        );
    }

    #[test]
    fn attempts_without_a_failure_record_are_counted_but_not_described() {
        let in_flight = HtlcAttempt {
            status: HtlcStatus::InFlight as i32,
            route: Some(route(&[PEER, PAYEE])),
            ..Default::default()
        };
        let payment = failed_payment(PaymentFailureReason::FailureReasonTimeout, vec![in_flight]);

        let failure = PayoutFailure::from_payment(&payment, 128);

        assert_eq!(
            failure.to_string(),
            "reason=FAILURE_REASON_TIMEOUT attempts=1 (no attempt reported a failure) \
             amount_sat=64287 fee_cap_sat=128"
        );
    }

    #[test]
    fn a_failing_hop_outside_the_route_is_reported_without_a_pubkey() {
        let payment = failed_payment(
            PaymentFailureReason::FailureReasonError,
            vec![failed_attempt(
                &[PEER],
                FailureCode::TemporaryChannelFailure,
                5,
            )],
        );

        let text = PayoutFailure::from_payment(&payment, 128).to_string();

        assert!(
            text.contains(
                "last_failure=TEMPORARY_CHANNEL_FAILURE at hop 5 of 1 (outside the route)"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_failure_never_carries_the_invoice_or_the_preimage() {
        let payment = failed_payment(
            PaymentFailureReason::FailureReasonIncorrectPaymentDetails,
            vec![failed_attempt(
                &[PEER, PAYEE],
                FailureCode::IncorrectOrUnknownPaymentDetails,
                2,
            )],
        );

        let text = PayoutFailure::from_payment(&payment, 128).to_string();

        assert!(!text.contains(INVOICE), "{text}");
        assert!(!text.contains(PREIMAGE), "{text}");
        assert!(text.contains(&format!("(node {PAYEE})")), "{text}");
    }
}
