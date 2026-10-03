use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;

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

    eprintln!("[browser] launching Chrome …");
    let (browser, mut handler) = Browser::launch(config).await?;

    // Drive the CDP connection in a background task
    let task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                eprintln!("[browser] CDP error: {e:?}");
            }
        }
    });

    eprintln!("[browser] ready");
    Ok((browser, task))
}

// ─── Entry Point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Res<()> {
    let (mut browser, driver_task) = launch_browser().await?;

    let page = browser.new_page("about:blank").await?;
    page.set_user_agent(USER_AGENT).await?;

    // ── Step 1: scrape the listing page ──────────────────────────────────────
    eprintln!("[main] step 1/2 — scraping listing page …");
    let items: Vec<Item> = scrape(&page).await?;
    eprintln!("[main] scraped {} items", items.len());

    // ── Step 2: fetch the first article ──────────────────────────────────────
    let item = items.first().expect("no items scraped");
    eprintln!("[main] step 2/2 — fetching first article …");
    eprintln!("[main]   title : {}", item.title);
    eprintln!("[main]   url   : {}", item.url);

    let article = fetch_one(&page, item).await?;
    println!("{}", serde_json::to_string_pretty(&article)?);

    // ── Cleanup ───────────────────────────────────────────────────────────────
    eprintln!("[main] shutting down …");
    page.close().await.ok();
    browser.close().await.ok();
    driver_task.await?;
    eprintln!("[main] done");

    Ok(())
}
