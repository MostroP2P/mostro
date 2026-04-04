// This example connects to LND and uses router rpc to track a payment.
//
// The program accepts four arguments: address, cert file, macaroon file, payment hash
// The address must start with `https://`!
//
// Example run: `cargo run --features=routerrpc --example track_payment <address> <tls.cert> <file.macaroon> <payment_hash>`

#[tokio::main]
#[cfg(feature = "routerrpc")]
async fn main() {
    let mut args = std::env::args_os();
    args.next().expect("not even zeroth arg given");
    let address: String = args
        .next()
        .expect("missing arguments: address, macaroon file, payment hash")
        .into_string()
        .expect("address is not UTF-8");
    let cert_file: String = args
        .next()
        .expect("missing arguments: cert file, macaroon file, payment hash")
        .into_string()
        .expect("cert_file is not UTF-8");
    let macaroon_file: String = args
        .next()
        .expect("missing argument: macaroon file, payment hash")
        .into_string()
        .expect("macaroon_file is not UTF-8");
    let payment_hash: Vec<u8> = hex::decode(
        args.next()
            .expect("missing argument: payment hash")
            .into_string()
            .expect("payment_hash is not UTF-8"),
    )
    .expect("payment_hash is not a valid hex");

    // Connecting to LND requires only address, cert file, and macaroon file
    let mut client = fedimint_tonic_lnd::connect(address, cert_file, macaroon_file)
        .await
        .expect("failed to connect");

    let response = client
        .router()
        .track_payment(fedimint_tonic_lnd::routerrpc::TrackPaymentRequest {
            payment_hash,
            no_inflight_updates: false,
        })
        .await
        .expect("Failed to call track_payment");

    if let Some(payment) = response
        .into_inner()
        .message()
        .await
        .expect("Failed to get payment")
    {
        println!("{:?}", payment);
    } else {
        println!("Payment not found");
    }
}
