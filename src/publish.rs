//! Publishing an event to the node's relays without waiting for the slowest.
//!
//! `Client::send_event` resolves only once **every** relay has answered `OK`
//! or reached its 10 s timeout, and nostr-sdk 0.45 has no first-success ack
//! policy. One relay that keeps its socket open and never answers therefore
//! costs every publish the full timeout. The event loop in `app.rs` handles
//! events one at a time and awaits order-book updates inline, and the reply
//! queue in `scheduler.rs` sends one message at a time, so that cost adds up:
//! requests waiting behind it outlived the 10 s freshness check and were
//! dropped, and replies reached clients after they had stopped waiting
//! (#991).
//!
//! [`send_event_first_ack`] is the one entry point for events whose sender
//! is waiting on the publish: daemon replies (`send_dm`) and order-book
//! updates.

use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use tokio::sync::{mpsc, oneshot};

/// Where one publication landed, per relay.
#[derive(Debug, Default)]
pub struct PublishReport {
    /// Relays that accepted the event (`OK true`).
    pub success: Vec<RelayUrl>,
    /// Relays that refused the event, timed out or were unreachable, with
    /// the reason.
    pub failed: Vec<(RelayUrl, String)>,
}

/// Publish `event` to every write relay of `client`, and resolve as soon as
/// the **first** relay accepts it.
///
/// Every relay still gets the event: each send runs in its own task, with
/// nostr-sdk's own per-relay handling (authentication, `OK` timeout), and
/// keeps running after this returns. `on_settled` receives the per-relay
/// [`PublishReport`] once every relay has answered or timed out, which may be
/// after this function has returned. It is called exactly once, also when no
/// relay accepted the event or there was no write relay at all.
///
/// Fails only when no relay accepted the event, that is once every relay has
/// refused or timed out, or at once when there is no write relay.
pub async fn send_event_first_ack<F>(
    client: &Client,
    event: &Event,
    on_settled: F,
) -> Result<(), MostroError>
where
    F: FnOnce(PublishReport) + Send + 'static,
{
    let urls: Vec<RelayUrl> = client
        .relays()
        .with_capabilities(RelayCapabilities::WRITE)
        .await
        .into_keys()
        .collect();

    // One task per relay, reporting its outcome on `outcomes`. Only the tasks
    // hold senders, so the channel closes once all of them have reported.
    let (outcomes_tx, mut outcomes) = mpsc::unbounded_channel();
    for url in urls {
        let (client, event, outcomes_tx) = (client.clone(), event.clone(), outcomes_tx.clone());
        tokio::spawn(async move {
            let outcome = send_to(&client, &event, &url).await;
            let _ = outcomes_tx.send((url, outcome));
        });
    }
    drop(outcomes_tx);

    // The collector owns the report, so it outlives this call: it wakes the
    // caller on the first acceptance and hands the report over at the end.
    let (first_ack_tx, first_ack) = oneshot::channel();
    tokio::spawn(async move {
        let mut first_ack_tx = Some(first_ack_tx);
        let mut report = PublishReport::default();
        while let Some((url, outcome)) = outcomes.recv().await {
            match outcome {
                Ok(()) => {
                    report.success.push(url);
                    if let Some(tx) = first_ack_tx.take() {
                        let _ = tx.send(());
                    }
                }
                Err(reason) => report.failed.push((url, reason)),
            }
        }
        // Dropping the sender unanswered tells the caller nobody accepted.
        drop(first_ack_tx);
        on_settled(report);
    });

    first_ack.await.map_err(|_| {
        MostroInternalErr(ServiceError::NostrError(
            "no relay accepted the event".to_string(),
        ))
    })
}

/// Send `event` to the single relay `url` and reduce the outcome to accepted
/// or the reason it was not.
async fn send_to(client: &Client, event: &Event, url: &RelayUrl) -> Result<(), String> {
    let output = client
        .send_event(event)
        .to([url])
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, reason)) = output.failed.into_iter().next() {
        return Err(reason);
    }
    if output.success.is_empty() {
        return Err("not sent".to_string());
    }
    Ok(())
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
        let client = connected_client(std::slice::from_ref(&silent_url)).await;
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
