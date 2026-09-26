//! Publishing an event to the node's relays.
//!
//! [`send_event_first_ack`] is the one entry point for events whose sender
//! is waiting on the publish: daemon replies (`send_dm`) and order-book
//! updates, which the event loop in `app.rs` awaits inline.

use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Where one publication landed, per relay.
#[derive(Debug, Default)]
pub struct PublishReport {
    /// Relays that accepted the event (`OK true`).
    pub success: Vec<RelayUrl>,
    /// Relays that refused the event, timed out or were unreachable, with
    /// the reason.
    pub failed: Vec<(RelayUrl, String)>,
}

/// Publish `event` to every write relay of `client`.
///
/// `on_settled` receives the per-relay [`PublishReport`] once every relay has
/// answered or timed out. Fails when no relay accepted the event.
pub async fn send_event_first_ack<F>(
    client: &Client,
    event: &Event,
    on_settled: F,
) -> Result<(), MostroError>
where
    F: FnOnce(PublishReport) + Send + 'static,
{
    let output = client
        .send_event(event)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    let report = PublishReport {
        success: output.success.into_keys().collect(),
        failed: output.failed.into_iter().collect(),
    };
    let accepted = !report.success.is_empty();
    on_settled(report);
    if accepted {
        Ok(())
    } else {
        Err(MostroInternalErr(ServiceError::NostrError(
            "no relay accepted the event".to_string(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::local_relay::{LocalRelay, MockRelay, WritePolicy, WritePolicyResult};
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::time::Duration;

    /// A relay that keeps the WebSocket open and never answers an `EVENT` —
    /// what `relay.mostro.network` did in #991.
    #[derive(Debug)]
    struct NeverAnswers;

    impl WritePolicy for NeverAnswers {
        fn admit_event<'a>(
            &'a self,
            _event: &'a Event,
            _addr: &'a SocketAddr,
        ) -> Pin<Box<dyn Future<Output = WritePolicyResult> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    async fn silent_relay() -> LocalRelay {
        let relay = LocalRelay::builder().write_policy(NeverAnswers).build();
        relay.run().await.expect("run silent relay");
        relay
    }

    async fn connected_client(urls: &[RelayUrl]) -> Client {
        let client = Client::default();
        for url in urls {
            client.add_relay(url.clone()).await.expect("add relay");
        }
        client.connect().and_wait(Duration::from_secs(5)).await;
        client
    }

    fn note() -> Event {
        EventBuilder::new(nostr_sdk::prelude::Kind::TextNote, "#991")
            .finalize(&Keys::generate())
            .expect("sign")
    }

    #[tokio::test]
    async fn a_silent_relay_does_not_hold_the_publish() {
        // Arrange
        let healthy = MockRelay::run().await.expect("mock relay");
        let silent = silent_relay().await;
        let (healthy_url, silent_url) = (healthy.url().await, silent.url().await);
        let client = connected_client(&[healthy_url.clone(), silent_url.clone()]).await;
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();

        // Act
        let published = tokio::time::timeout(
            Duration::from_secs(3),
            send_event_first_ack(&client, &note(), move |report| {
                let _ = report_tx.send(report);
            }),
        )
        .await;

        // Assert
        assert!(
            matches!(published, Ok(Ok(()))),
            "the healthy relay's OK must end the wait, not the silent relay's timeout"
        );
        let report = tokio::time::timeout(Duration::from_secs(20), report_rx)
            .await
            .expect("every relay settles within its OK timeout")
            .expect("report delivered");
        assert_eq!(report.success, vec![healthy_url]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, silent_url);
    }

    #[tokio::test]
    async fn fails_when_no_relay_accepts() {
        // Arrange
        let silent = silent_relay().await;
        let silent_url = silent.url().await;
        let client = connected_client(&[silent_url.clone()]).await;
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();

        // Act
        let published = send_event_first_ack(&client, &note(), move |report| {
            let _ = report_tx.send(report);
        })
        .await;

        // Assert
        assert!(published.is_err());
        let report = report_rx.await.expect("report delivered");
        assert!(report.success.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, silent_url);
    }
}
