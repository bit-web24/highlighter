use crate::scrap::{Item, Res, wait_for};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use tokio::time::{Duration, sleep};
use tracing::{info, warn, error, instrument};

// ─── Constants ───────────────────────────────────────────────────────────────

/// CSS selector for the article content div.
/// Note: "innner" (3 n's) matches the real markup on pib.gov.in.
const CONTENT_SELECTOR: &str = ".innner-page-main-about-us-content-right-part";

/// Max retry attempts when fetching a single article.
const MAX_ATTEMPTS: u64 = 3;

/// Seconds to wait between retries when blocked.
const BLOCKED_BACKOFF_SECS: u64 = 30;

/// Pause between consecutive article fetches to be polite to the server.
const INTER_FETCH_PAUSE_MS: u64 = 1000;

// ─── Data Types ──────────────────────────────────────────────────────────────

// Lang is deserialized from JS via serde_json, not constructed directly in Rust
#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize)]
struct Lang {
    lang: String,
    url: String,
}

/// Structured content extracted from a single press-release page.
#[derive(Debug, Deserialize, Serialize)]
pub struct Detail {
    pub ministry: String,
    pub title: String,
    pub subtitle: String,
    pub published: String,  // e.g. "30 SEP 2026 7:15PM"
    pub source: String,     // e.g. "PIB Delhi"
    pub release_id: String,
    pub body: String,       // plain text, paragraphs joined by blank lines
}

/// A `Detail` paired with its canonical URL (for serialization / storage).
#[derive(Debug, Serialize)]
pub struct Article {
    pub url: String,
    #[serde(flatten)]
    pub detail: Detail,
}

// ─── JavaScript Extractor ────────────────────────────────────────────────────

/// Runs inside the article page to pull out all structured fields.
const DETAIL_JS: &str = r#"
(() => {
  const root = document.querySelector('.innner-page-main-about-us-content-right-part');
  const clean = s => (s || '').replace(/\u00a0/g, ' ').replace(/[ \t]+/g, ' ').replace(/\s*\n\s*/g, '\n').trim();
  const txt = sel => { const e = root.querySelector(sel); return e ? clean(e.innerText) : ''; };

  // Elements that belong to the header / sidebar, not the body text
  const skipIds     = new Set(['MinistryName','lg_g','PrDateTime','reel_pic','ReleaseId','lblViews','lblRefPhoto','RelLink']);
  const skipClasses = ['event-heading-background','pt20','BackgroundRelease','ReleaseLang','RelTag','clear'];
  const skipTags    = new Set(['BR','SPAN','INPUT','CENTER','IMG','SCRIPT','STYLE']);

  const parts = [];
  for (const el of root.children) {
    if (skipIds.has(el.id) || skipTags.has(el.tagName)) continue;
    if (skipClasses.some(c => el.classList.contains(c))) continue;
    const t = clean(el.innerText);
    if (!t) continue;
    parts.push(t);
  }

  const posted = txt('#PrDateTime').replace(/^Posted On:\s*/i, '');
  const m = posted.match(/(\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s+\d{1,2}:\d{2}\s*[AaPp][Mm])(?:\s+by\s+(.+))?/);

  return {
    ministry:   txt('#MinistryName'),
    title:      txt('#Titleh2'),
    subtitle:   txt('#Subtitleh3'),
    published:  m ? m[1] : posted,
    source:     m && m[2] ? m[2].trim() : '',
    release_id: (txt('#ReleaseId').match(/\d+/) || [''])[0],
    body:       parts.join('\n\n'),
    languages:  Array.from(root.querySelectorAll('.ReleaseLang a'))
                  .map(a => ({ lang: clean(a.textContent), url: a.href })),
  };
})()
"#;

// ─── URL Helpers ─────────────────────────────────────────────────────────────

/// Converts any PIB press-release URL to the inner page that hosts the article HTML.
///
/// The outer `PressReleaseDetail.aspx` loads content inside an `<iframe>`, so
/// `.innner-page-main-about-us-content-right-part` is only findable on the inner page.
///
/// Both input shapes are handled:
/// - `PressReleaseDetail.aspx?PRID=NNN`  →  `PressReleasePage.aspx?PRID=NNN`
/// - `PressReleasePage.aspx?PRID=NNN`    →  (unchanged)
fn inner_url(url: &str) -> String {
    if let Some(prid_start) = url.to_ascii_lowercase().find("prid=") {
        let after = &url[prid_start + 5..];
        let prid = after
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or(after);
        if !prid.is_empty() {
            return format!("https://www.pib.gov.in/PressReleasePage.aspx?PRID={prid}");
        }
    }
    // Fallback — should never happen for well-formed PIB URLs
    url.to_string()
}

// ─── Retry / Backoff Helper ───────────────────────────────────────────────────

/// Returns how long to wait (seconds) before retrying after an error.
fn backoff_secs(err: &str, attempt: u64) -> u64 {
    if err.contains("blocked") {
        BLOCKED_BACKOFF_SECS
    } else {
        2 * attempt
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Navigates to the inner article page and extracts its structured content.
#[instrument(skip(page), fields(url))]
pub async fn fetch_content(page: &Page, url: &str) -> Res<Detail> {
    let target = inner_url(url);
    info!(target = %target, "navigating to article page");
    page.goto(&target).await?;

    // Wait for content div OR block page — failing fast on bot-detection
    wait_for(
        page,
        &format!(
            "document.readyState === 'complete' && \
             (!!document.querySelector('{CONTENT_SELECTOR}') || \
              document.title.includes('Access Denied'))"
        ),
        75,
    )
    .await
    .map_err(|e| format!("content never appeared: {e}"))?;

    let title: String = page.evaluate("document.title").await?.into_value()?;
    if title.contains("Access Denied") {
        error!("blocked by bot protection");
        return Err("blocked by bot protection".into());
    }

    let detail: Detail = page.evaluate(DETAIL_JS).await?.into_value()?;

    if detail.body.is_empty() {
        return Err("empty body (image-only release or layout change?)".into());
    }
    if detail.title.is_empty() || detail.published.is_empty() {
        warn!(url, "article is missing title or date");
    }

    info!(title = %detail.title, "article fetched");
    Ok(detail)
}

/// Fetches a single article with up to `MAX_ATTEMPTS` retries.
#[instrument(skip(page), fields(url = %item.url))]
pub async fn fetch_one(page: &Page, item: &Item) -> Res<Article> {
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "fetching article");
        match fetch_content(page, &item.url).await {
            Ok(detail) => {
                return Ok(Article {
                    url: item.url.clone(),
                    detail,
                });
            }
            Err(e) => {
                last_err = e.to_string();
                let wait = backoff_secs(&last_err, attempt);
                warn!(attempt, error = %last_err, wait_secs = wait, "attempt failed, retrying");
                sleep(Duration::from_secs(wait)).await;
            }
        }
    }

    error!(max = MAX_ATTEMPTS, error = %last_err, "all attempts exhausted");
    Err(format!("failed after {MAX_ATTEMPTS} attempts: {last_err}").into())
}

/// Fetches all articles from `items`, appending results (as NDJSON) to `out_path`.
/// Already-fetched URLs (present in the file) are skipped automatically.
#[instrument(skip(page, items), fields(out_path, total = items.len()))]
pub async fn fetch_all(page: &Page, items: &[Item], out_path: &str) -> Res<(usize, usize)> {
    // Build a set of URLs already written to the output file
    let done: HashSet<String> = std::fs::read_to_string(out_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["url"].as_str().map(String::from))
        .collect();

    let skipped = done.len();
    let remaining = items.iter().filter(|i| !done.contains(&i.url)).count();
    info!(skipped, remaining, out_path, "starting fetch_all");

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_path)?;

    let (mut ok, mut failed) = (0usize, 0usize);
    let total = items.len();

    for (n, item) in items.iter().enumerate() {
        if done.contains(&item.url) {
            continue;
        }

        info!(n = n + 1, total, url = %item.url, "fetching item");

        let mut detail = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match fetch_content(page, &item.url).await {
                Ok(d) => {
                    detail = Some(d);
                    break;
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let wait = backoff_secs(&err_str, attempt);
                    warn!(
                        n = n + 1, total, attempt, max = MAX_ATTEMPTS,
                        error = %err_str, wait_secs = wait,
                        "attempt failed, retrying"
                    );
                    sleep(Duration::from_secs(wait)).await;
                }
            }
        }

        match detail {
            Some(detail) => {
                let article = Article { url: item.url.clone(), detail };
                writeln!(file, "{}", serde_json::to_string(&article)?)?;
                file.flush()?; // flush after each write to survive a crash mid-run
                ok += 1;
                info!(n = n + 1, total, "item saved");
            }
            None => {
                error!(n = n + 1, total, url = %item.url, "gave up on item after all attempts");
                failed += 1;
            }
        }

        sleep(Duration::from_millis(INTER_FETCH_PAUSE_MS)).await; // be polite
    }

    info!(ok, failed, "fetch_all complete");
    Ok((ok, failed))
}
