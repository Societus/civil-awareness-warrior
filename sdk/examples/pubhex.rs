//! Operator debug: print the 64-hex public half of a PKCS#8 ed25519 PEM.
//! Same code path as the GUI's "Derive public key" button
//! (load_signing_key_pem + signing_key_public_hex), usable from a terminal:
//!   cargo run -p lh-contributor-client --example pubhex -- /path/to/key.pem
//! Cross-check with:
//!   openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | xxd -p -c 64
use lh_contributor_client::crypto::{load_signing_key_pem, signing_key_public_hex};

fn main() {
    let path = std::env::args().nth(1).expect("usage: pubhex <key.pem>");
    let pem = std::fs::read_to_string(&path).expect("read pem");
    match load_signing_key_pem(&pem) {
        Ok(k) => println!("{}", signing_key_public_hex(&k)),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
