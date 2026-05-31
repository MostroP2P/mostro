// This example connects to LND and prints out all incoming invoices as they settle.
//
// The program accepts three arguments: address, cert file, macaroon file
// The address must start with `https://`!
//
// Example run: `cargo run --features=lightningrpc --example subscribe_invoices <address> <tls.cert> <file.macaroon>`

#[tokio::main]
#[cfg(feature = "lightningrpc")]
async fn main() {
    let mut args = std::env::args_os();
    args.next().expect("not even zeroth arg given");
    let address = args
        .next()
        .expect("missing arguments: address, cert file, macaroon file");
    let cert_file = args
        .next()
        .expect("missing arguments: cert file, macaroon file");
    let macaroon_file = args.next().expect("missing argument: macaroon file");
    let address = address.into_string().expect("address is not UTF-8");

    // Connecting to LND requires only address, cert file, and macaroon file
    let mut client = fedimint_tonic_lnd::connect(address, cert_file, macaroon_file)
        .await
        .expect("failed to connect");

    let mut invoice_stream = client
        .lightning()
        .subscribe_invoices(fedimint_tonic_lnd::lnrpc::InvoiceSubscription {
            add_index: 0,
            settle_index: 0,
        })
        .await
        .expect("Failed to call subscribe_invoices")
        .into_inner();

    while let Some(invoice) = invoice_stream
        .message()
        .await
        .expect("Failed to receive invoices")
    {
        let state: fedimint_tonic_lnd::lnrpc::invoice::InvoiceState = invoice
            .state
            .try_into()
            .expect("Failed to parse invoice state");

        // If this invoice was Settled we can do something with it
        if state == fedimint_tonic_lnd::lnrpc::invoice::InvoiceState::Settled {
            println!("{:?}", invoice);
        }
    }
}
