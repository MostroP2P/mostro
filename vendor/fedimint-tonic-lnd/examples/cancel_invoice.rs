// This example connects to LND and uses invoices rpc to cancel an existing invoice
//
// The program accepts four arguments: address, cert file, macaroon file, payment hash
// The address must start with `https://`!
//
// Example run: `cargo run --features=invoicesrpc --example cancel_invoice <address> <tls.cert> <file.macaroon> <payment_hash>`

#[tokio::main]
#[cfg(feature = "invoicesrpc")]
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

    client
        .invoices()
        .cancel_invoice(fedimint_tonic_lnd::invoicesrpc::CancelInvoiceMsg { payment_hash })
        .await
        .expect("Failed to cancel invoice");

    println!("Invoice canceled");
}
