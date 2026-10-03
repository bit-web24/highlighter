use chromiumoxide::Page;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

const URL: &str = "https://www.pib.gov.in/allRel.aspx?reg=48&lang=1";

#[derive(Debug, Deserialize)]
pub struct Item {
    pub url: String,
    pub title: String,
    pub ministry: String,
    pub published: String,
}

// walks headings + release links in document order. a heading sets the
// current ministry, each PRID link takes the latest one seen above it
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

// poll a js expression until it's true. no cdp node ids, so it survives reloads
pub async fn wait_for(page: &Page, js: &str, tries: u32) -> Res<()> {
    for _ in 0..tries {
        // evaluate can error mid-navigation, just keep polling
        if let Ok(r) = page.evaluate(js).await {
            if let Ok(true) = r.into_value::<bool>() {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(format!("timed out waiting for: {js}").into())
}

pub async fn dump_html(page: &Page) -> Res<()> {
    let html: String = page
        .evaluate("document.documentElement.outerHTML")
        .await?
        .into_value()?;
    std::fs::write("dump.html", html)?;
    Ok(())
}

pub async fn get_value(page: &Page, id: &str) -> Res<String> {
    Ok(page
        .evaluate(format!("document.getElementById('{id}').value"))
        .await?
        .into_value()?)
}

pub async fn set_select(page: &Page, id: &str, value: &str) -> Res<()> {
    if get_value(page, id).await? == value {
        return Ok(()); // already set, no postback needed
    }

    // marker disappears when the postback reloads the page
    page.evaluate("window.__marker = true").await?;
    page.evaluate(format!(
        r#"(() => {{
            const s = document.getElementById('{id}');
            s.value = '{value}';
            s.dispatchEvent(new Event('change', {{ bubbles: true }}));
        }})()"#
    ))
    .await?;

    // wait until the old document is gone (marker cleared)
    wait_for(page, "window.__marker !== true", 100).await?;

    // wait until the new document is ready and has the select again
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
    Ok(())
}

pub async fn scrape(page: &Page) -> Res<Vec<Item>> {
    page.goto(URL).await?;

    wait_for(
        page,
        "document.readyState === 'complete' && !!document.getElementById('ContentPlaceHolder1_ddlMinistry')",
        100,
    )
    .await
    .map_err(|e| format!("form never appeared (blocked?): {e}"))?;

    let title: String = page.evaluate("document.title").await?.into_value()?;
    if title.contains("Access Denied") {
        return Err("blocked by bot protection".into());
    }

    // one postback (full reload) per change
    set_select(page, "ContentPlaceHolder1_ddlMinistry", "0").await?; // all ministry
    set_select(page, "ContentPlaceHolder1_ddlday", "0").await?; // all days
    set_select(page, "ContentPlaceHolder1_ddlMonth", "9").await?; // september
    set_select(page, "ContentPlaceHolder1_ddlYear", "2026").await?;

    if wait_for(page, "!!document.querySelector('.content-area')", 100)
        .await
        .is_err()
    {
        dump_html(page).await?;
        return Err("no .content-area, saved dump.html".into());
    }

    let items: Vec<Item> = page.evaluate(EXTRACT_JS).await?.into_value()?;

    if items.is_empty() {
        dump_html(page).await?;
        return Err("0 items (no PRID links?), saved dump.html".into());
    }

    // fail loud if the markup guesses were wrong
    let no_ministry = items.iter().filter(|i| i.ministry.is_empty()).count();
    let no_date = items.iter().filter(|i| i.published.is_empty()).count();
    if no_ministry > 0 || no_date > 0 {
        eprintln!(
            "warning: {no_ministry}/{} missing ministry, {no_date}/{} missing date, saved dump.html",
            items.len(),
            items.len()
        );
        dump_html(page).await?;
    }

    Ok(items)
}
