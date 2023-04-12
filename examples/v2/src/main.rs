use std::{io::{self, Write}, time::{SystemTime, UNIX_EPOCH, Duration}, sync::atomic::{AtomicUsize, Ordering}};

use arctic::{
    v2::{EventHandler, EventType, PolarSensor},
    PmdRead,
};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    sync::{Mutex, oneshot},
};
use tracing::instrument;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

#[tokio::main]
#[instrument]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "v2=INFO,arctic=INFO".into()))
        .init();

    let id = get_id()?;

    let polar_handle = PolarSensor::new()
        .await?
        .block_connect(&id)
        .await?
        .listen(EventType::Acc)
        .listen(EventType::Ecg)
        .range(8)
        .sample_rate(200)
        .build()
        .await?
        .event_loop(Handler::new().await?)
        .await;
    tracing::info!("started event loop");

    get_finish().await?;
    polar_handle.stop().await;
    
    tracing::info!("stopped the event loop, finishing");

    Ok(())
}

#[derive(Debug)]
struct Handler {
    output: Mutex<File>,
}

impl Handler {
    async fn new() -> color_eyre::Result<Self> {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        Ok(Self {
            output: Mutex::new(File::create(format!("output/pmd_{unix}.txt")).await?),
        })
    }
}

#[arctic::async_trait]
impl EventHandler for Handler {
    #[instrument(skip_all)]
    async fn measurement_update(&self, data: PmdRead) {
        let mut lock = self.output.lock().await;
        let msg = format!("{data:?}\n");
        tracing::debug!("received data: {data:?}");
        lock.write_all(msg.as_bytes()).await.unwrap();

        COUNTER.fetch_add(1, Ordering::SeqCst);
    }
}

fn get_id() -> color_eyre::Result<String> {
    let mut id = String::new();

    print!("Device ID: ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut id)?;

    Ok(id.trim().to_string())
}

async fn get_finish() -> color_eyre::Result<()> {
    let mut buf = String::new();
    let (tx, mut rx) = oneshot::channel();

    let task = tokio::task::spawn(async move {
        println!();
        loop {
            if let Ok(_) = rx.try_recv() {
                return;
            }
            print!("\r({} events received) Would you like to stop? (y/N) ", COUNTER.load(Ordering::SeqCst));
            io::stdout().flush().unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    loop {
        io::stdin().read_line(&mut buf)?;
        if buf.trim().to_ascii_lowercase() == "y" {
            let _ = tx.send(());
            task.await?;
            return Ok(());
        }
    }
}
