use std::io::{self, Write};

use arctic::v2::PolarSensor;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer().without_time())
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "characteristics=INFO,arctic=INFO".into()),
        )
        .init();

    let id = get_id();
    let connected = PolarSensor::new()
        .await
        .unwrap()
        .block_connect(&id)
        .await
        .unwrap();

    tracing::info!("connected, printing characteristics");

    let characteristics = connected.characteristics();

    for char in characteristics {
        tracing::info!("characteristic: {char:?}");
    }

    tracing::info!("finished printing characteristics");
}

fn get_id() -> String {
    print!("Device ID: ");
    io::stdout().flush().unwrap();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap();

    buf.trim().to_string()
}
