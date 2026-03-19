// This example fetches and prints the version info of the running lnd daemon
//
// The program accepts three arguments: address, cert file, macaroon file
// The address must start with `https://`!
//
// Example run: `cargo run --features=versionrpc --example get_version <address> <tls.cert> <file.macaroon>`

#[tokio::main]
#[cfg(feature = "versionrpc")]
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

    let version = client
        .versioner()
        .get_version(fedimint_tonic_lnd::verrpc::VersionRequest {})
        .await
        .expect("failed to get version");
    // We only print it here, note that in real-life code you may want to call `.into_inner()` on
    // the response to get the message.
    println!("{:#?}", version);
}
