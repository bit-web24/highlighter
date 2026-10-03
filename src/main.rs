use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;

pub mod report;
pub mod scrap;

use report::{Detail, fetch_content};
use scrap::{Item, Res, scrape};
use tokio::time::{Duration, sleep};

use crate::report::fetch_one;

#[tokio::main]
async fn main() -> Res<()> {
    let config = BrowserConfig::builder()
        .chrome_executable("/usr/bin/google-chrome")
        .user_data_dir("/home/bittu/.cache/crawl-profile")
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--disable-gpu")
        .build()?;

    let (mut browser, mut handler) = Browser::launch(config).await?;

    // drive the connection, don't forget this
    let task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                eprintln!("Browser Error: {:?}", e);
            }
        }
    });

    let page = browser.new_page("about:blank").await?;
    page.set_user_agent(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    )
    .await?;

    let result = scrape(&page).await;

    let items: Vec<Item> = result?;
    println!("{} items", items.len());
    // for i in &items {
    //     println!(
    //         "{} | {} | {}\n  {}",
    //         i.ministry, i.published, i.title, i.url
    //     );
    // }

    let item = items.iter().next().unwrap();

    let article = fetch_one(&page, item).await?;

    println!("{}", serde_json::to_string_pretty(&article)?);

    // always clean up, even if scrape failed
    page.close().await.ok();
    browser.close().await.ok();
    task.await?;

    Ok(())
}
