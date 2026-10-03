use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use tracing::{info, error, Level};
use tracing_subscriber::EnvFilter;

pub mod report;
pub mod scrap;

use report::fetch_one;
use scrap::{Item, Res, scrape};

// ─── Browser Setup ───────────────────────────────────────────────────────────

const CHROME_BIN: &str = "/usr/bin/google-chrome";
const PROFILE_DIR: &str = "/home/bittu/.cache/crawl-profile";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

async fn launch_browser() -> Res<(Browser, tokio::task::JoinHandle<()>)> {
    let config = BrowserConfig::builder()
        .chrome_executable(CHROME_BIN)
        .user_data_dir(PROFILE_DIR)
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--disable-gpu")
        .build()?;

    info!("launching Chrome");
    let (browser, mut handler) = Browser::launch(config).await?;

    // Drive the CDP connection in a background task
    let task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                error!(error = ?e, "CDP connection error");
            }
        }
    });

    info!("browser ready");
    Ok((browser, task))
}

// ─── Entry Point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Res<()> {
    // Initialise tracing — respects RUST_LOG env var, defaults to INFO
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(Level::INFO.as_str())),
        )
        .with_target(false) // don't print module paths
        .init();

    let (mut browser, driver_task) = launch_browser().await?;

    let page = browser.new_page("about:blank").await?;
    page.set_user_agent(USER_AGENT).await?;

    // ── Step 1: scrape the listing page ──────────────────────────────────────
    info!("step 1/2 — scraping listing page");
    let items: Vec<Item> = scrape(&page).await?;
    info!(count = items.len(), "listing scraped");

    // ── Step 2: fetch the first article ──────────────────────────────────────
    let item = items.first().expect("no items scraped");
    info!(
        title = %item.title,
        url   = %item.url,
        "step 2/2 — fetching first article"
    );

    let article = fetch_one(&page, item).await?;
    println!("{}", serde_json::to_string_pretty(&article)?);

    // ── Cleanup ───────────────────────────────────────────────────────────────
    info!("shutting down");
    page.close().await.ok();
    browser.close().await.ok();
    driver_task.await?;
    info!("done");

    Ok(())
}
