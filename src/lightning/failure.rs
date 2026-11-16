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
