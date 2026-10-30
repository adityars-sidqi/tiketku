//! Payments and payment gateway webhooks for TiketKu.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!(
        "{}",
        common::startup_message(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    );
    Ok(())
}
