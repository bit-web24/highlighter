use chromiumoxide::Page;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn, error};

// ─── Types ───────────────────────────────────────────────────────────────────

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// One press-release entry scraped from the listing page.
#[derive(Debug, Deserialize)]
pub struct Item {
    pub url: String,
    pub title: String,
    pub ministry: String,
    pub published: String,
}

// ─── Constants ───────────────────────────────────────────────────────────────

const LISTING_URL: &str = "https://www.pib.gov.in/allRel.aspx?reg=48&lang=1";

/// Walks headings + release links in document order.
/// A heading sets the current ministry; each PRID link takes the latest one seen above it.
const EXTRACT_JS: &str = r#"
(() => {
  const root = document.querySelector('.content-area');
  const out = [];
  const seen = new Set();
  let ministry = '';
  const clean = s => s.replace(/\s+/g, ' ').trim();
  const dateRe = /\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}(?:\s+\d{1,2}:\d{2}\s*[AaPp][Mm])?/;
  for (const el of root.querySelectorAll('h1,h2,h3,h4,h5,h6,a[href*="PRID="]')) {
    if (/^H[1-6]$/.test(el.tagName)) { ministry = clean(el.textContent); continue; }
    if (seen.has(el.href)) continue;
    seen.add(el.href);
    const box = el.closest('li') || el.parentElement;
    const dateEl = box.querySelector('[class*="publishdate"], [class*="date"]');
    const raw = dateEl ? dateEl.textContent : box.textContent.replace(el.textContent, '');
    const m = raw.match(dateRe);
    out.push({
      url: el.href,
      title: clean(el.textContent),
      ministry,
      published: m ? m[0] : clean(raw),
    });
  }
  return out;
})()
"#;

// ─── Browser Helpers ─────────────────────────────────────────────────────────

/// Polls a JS expression (200 ms interval) until it evaluates to `true`.
/// Uses no CDP node IDs, so it survives page reloads.
pub async fn wait_for(page: &Page, js: &str, tries: u32) -> Res<()> {
    for _ in 0..tries {
        // evaluate() can error mid-navigation — just keep polling
        if let Ok(r) = page.evaluate(js).await {
            if let Ok(true) = r.into_value::<bool>() {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(format!("timed out waiting for: {js}").into())
}

/// Dumps the current page's full HTML to `dump.html` for debugging.
pub async fn dump_html(page: &Page) -> Res<()> {
    let html: String = page
        .evaluate("document.documentElement.outerHTML")
        .await?
        .into_value()?;
    std::fs::write("dump.html", html)?;
    debug!("page HTML saved → dump.html");
    Ok(())
}

/// Returns the current value of a `<select>` / `<input>` element by its `id`.
pub async fn get_value(page: &Page, id: &str) -> Res<String> {
    Ok(page
        .evaluate(format!("document.getElementById('{id}').value"))
        .await?
        .into_value()?)
}

/// Sets a `<select>` element to `value` and waits for the ASP.NET postback to finish.
/// No-ops if the element already has the desired value.
pub async fn set_select(page: &Page, id: &str, value: &str) -> Res<()> {
    let current = get_value(page, id).await?;
    if current == value {
        debug!(id, value, "select already at target, skipping postback");
        return Ok(());
    }

    info!(id, from = %current, to = %value, "setting select (postback)");

    // Plant a marker that disappears when the postback reloads the page
    page.evaluate("window.__marker = true").await?;
    page.evaluate(format!(
        r#"(() => {{
            const s = document.getElementById('{id}');
            s.value = '{value}';
            s.dispatchEvent(new Event('change', {{ bubbles: true }}));
        }})()"#
    ))
    .await?;

    // Wait until the old document is gone (marker cleared by reload)
    wait_for(page, "window.__marker !== true", 100).await?;

    // Wait until the new document is ready and the select is back
    wait_for(
        page,
        &format!("document.readyState === 'complete' && !!document.getElementById('{id}')"),
        100,
    )
    .await?;

    let now = get_value(page, id).await?;
    if now != value {
        return Err(format!("{id} is '{now}', wanted '{value}'").into());
    }

    debug!(id, value, "select confirmed");
    Ok(())
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Navigates to the PIB listing page, applies filters, and returns all press-release items.
pub async fn scrape(page: &Page) -> Res<Vec<Item>> {
    info!(url = LISTING_URL, "opening listing page");
    page.goto(LISTING_URL).await?;

    info!("waiting for filter form");
    wait_for(
        page,
        "document.readyState === 'complete' && !!document.getElementById('ContentPlaceHolder1_ddlMinistry')",
        100,
    )
    .await
    .map_err(|e| format!("form never appeared (blocked?): {e}"))?;

    let title: String = page.evaluate("document.title").await?.into_value()?;
    if title.contains("Access Denied") {
        error!("blocked by bot protection");
        return Err("blocked by bot protection".into());
    }
    debug!(page_title = %title, "listing page loaded");

    // Apply filters — each triggers one ASP.NET postback (full reload)
    info!("applying filters");
    set_select(page, "ContentPlaceHolder1_ddlMinistry", "0").await?; // all ministries
    set_select(page, "ContentPlaceHolder1_ddlday", "0").await?;       // all days
    set_select(page, "ContentPlaceHolder1_ddlMonth", "9").await?;     // september
    set_select(page, "ContentPlaceHolder1_ddlYear", "2026").await?;

    info!("waiting for .content-area");
    if wait_for(page, "!!document.querySelector('.content-area')", 100)
        .await
        .is_err()
    {
        dump_html(page).await?;
        return Err("no .content-area, saved dump.html".into());
    }

    info!("extracting items");
    let items: Vec<Item> = page.evaluate(EXTRACT_JS).await?.into_value()?;

    if items.is_empty() {
        dump_html(page).await?;
        return Err("0 items (no PRID links?), saved dump.html".into());
    }

    // Warn if the markup guesses were wrong for any items
    let no_ministry = items.iter().filter(|i| i.ministry.is_empty()).count();
    let no_date = items.iter().filter(|i| i.published.is_empty()).count();
    if no_ministry > 0 || no_date > 0 {
        warn!(
            no_ministry,
            no_date,
            total = items.len(),
            "some items are missing ministry or date"
        );
        dump_html(page).await?;
    }

    info!(count = items.len(), "scrape complete");
    Ok(items)
}
