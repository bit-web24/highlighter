use crate::scrap::{Item, Res, wait_for};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use tokio::time::{Duration, sleep};

// "innner" (3 n's) matches the real markup
const CONTENT_SELECTOR: &str = ".innner-page-main-about-us-content-right-part";

#[derive(Debug, Deserialize, Serialize)]
struct Lang {
    lang: String,
    url: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Detail {
    pub ministry: String,
    pub title: String,
    pub subtitle: String,
    pub published: String, // e.g. "30 SEP 2026 7:15PM"
    pub source: String,    // e.g. "PIB Delhi"
    pub release_id: String,
    pub body: String, // plain text, paragraphs joined by blank lines
                      // pub body_html: String, // original markup, in case you want it later
}

#[derive(Debug, Serialize)]
pub struct Article {
    pub url: String,
    #[serde(flatten)]
    pub detail: Detail,
}

const DETAIL_JS: &str = r#"
(() => {
  const root = document.querySelector('.innner-page-main-about-us-content-right-part');
  const clean = s => (s || '').replace(/\u00a0/g, ' ').replace(/[ \t]+/g, ' ').replace(/\s*\n\s*/g, '\n').trim();
  const txt = sel => { const e = root.querySelector(sel); return e ? clean(e.innerText) : ''; };

  // everything that isn't body
  const skipIds = new Set(['MinistryName','lg_g','PrDateTime','reel_pic','ReleaseId','lblViews','lblRefPhoto','RelLink']);
  const skipClasses = ['event-heading-background','pt20','BackgroundRelease','ReleaseLang','RelTag','clear'];
  const skipTags = new Set(['BR','SPAN','INPUT','CENTER','IMG','SCRIPT','STYLE']);

  const parts = [], htmls = [];
  for (const el of root.children) {
    if (skipIds.has(el.id) || skipTags.has(el.tagName)) continue;
    if (skipClasses.some(c => el.classList.contains(c))) continue;
    const t = clean(el.innerText);
    if (!t) continue;
    parts.push(t);
    htmls.push(el.outerHTML);
  }

  const posted = txt('#PrDateTime').replace(/^Posted On:\s*/i, '');
  const m = posted.match(/(\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s+\d{1,2}:\d{2}\s*[AaPp][Mm])(?:\s+by\s+(.+))?/);

  return {
    ministry: txt('#MinistryName'),
    title: txt('#Titleh2'),
    subtitle: txt('#Subtitleh3'),
    published: m ? m[1] : posted,
    source: m && m[2] ? m[2].trim() : '',
    release_id: (txt('#ReleaseId').match(/\d+/) || [''])[0],
    body: parts.join('\n\n'),
    // body_html: htmls.join('\n'),
  };
})()
"#;

/// Convert any PIB press-release URL to the inner page that actually contains
/// the article markup.  Both shapes are handled:
///   PressReleaseDetail.aspx?PRID=NNN  →  PressReleasePage.aspx?PRID=NNN
///   PressReleasePage.aspx?PRID=NNN    →  (unchanged)
fn inner_url(url: &str) -> String {
    // Extract PRID value and build the direct inner-page URL.
    if let Some(prid_start) = url.to_ascii_lowercase().find("prid=") {
        let after = &url[prid_start + 5..];
        let prid: &str = after
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or(after);
        if !prid.is_empty() {
            return format!("https://www.pib.gov.in/PressReleasePage.aspx?PRID={prid}");
        }
    }
    // Fallback: return as-is (should never happen for well-formed PIB URLs).
    url.to_string()
}

pub async fn fetch_content(page: &Page, url: &str) -> Res<Detail> {
    // Navigate to the inner page directly — the outer PressReleaseDetail.aspx
    // loads article content inside an <iframe> (PressReleasePage.aspx?PRID=…),
    // so document.querySelector('.innner-page-main-about-us-content-right-part')
    // never matches in the outer document.
    let target = inner_url(url);
    page.goto(&target).await?;

    // wait for the content div OR the block page, so a block fails fast
    wait_for(
        page,
        &format!(
            "document.readyState === 'complete' && (!!document.querySelector('{CONTENT_SELECTOR}') || document.title.includes('Access Denied'))"
        ),
        75,
    )
    .await
    .map_err(|e| format!("content never appeared: {e}"))?;

    let title: String = page.evaluate("document.title").await?.into_value()?;
    if title.contains("Access Denied") {
        return Err("blocked by bot protection".into());
    }

    let detail: Detail = page.evaluate(DETAIL_JS).await?.into_value()?;

    if detail.body.is_empty() {
        return Err("empty body (image-only release or layout change?)".into());
    }
    if detail.title.is_empty() || detail.published.is_empty() {
        eprintln!("warning: missing title/date for {url}");
    }
    Ok(detail)
}

pub async fn fetch_one(page: &Page, item: &Item) -> Res<Article> {
    let mut last_err = String::new();

    for attempt in 1..=3u64 {
        match fetch_content(page, &item.url).await {
            Ok(detail) => {
                return Ok(Article {
                    url: item.url.clone(),
                    detail,
                });
            }
            Err(e) => {
                last_err = e.to_string();
                eprintln!("attempt {attempt} failed: {} ({last_err})", item.url);
                // blocked = back off hard, anything else = short backoff
                let wait = if last_err.contains("blocked") {
                    30
                } else {
                    2 * attempt
                };
                sleep(Duration::from_secs(wait)).await;
            }
        }
    }
    Err(format!("failed after 3 attempts: {last_err}").into())
}

// appends one json object per line, skips urls already in the file
pub async fn fetch_all(page: &Page, items: &[Item], out_path: &str) -> Res<(usize, usize)> {
    let done: HashSet<String> = std::fs::read_to_string(out_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["url"].as_str().map(String::from))
        .collect();

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_path)?;

    let (mut ok, mut failed) = (0, 0);

    for (n, item) in items.iter().enumerate() {
        if done.contains(&item.url) {
            continue;
        }

        let mut detail = None;
        for attempt in 1..=3u64 {
            match fetch_content(page, &item.url).await {
                Ok(d) => {
                    detail = Some(d);
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "[{}/{}] attempt {attempt} failed: {} ({e})",
                        n + 1,
                        items.len(),
                        item.url
                    );
                    // blocked = back off hard, anything else = short backoff
                    let wait = if e.to_string().contains("blocked") {
                        30
                    } else {
                        2 * attempt
                    };
                    sleep(Duration::from_secs(wait)).await;
                }
            }
        }

        match detail {
            Some(detail) => {
                let article = Article {
                    url: item.url.clone(),
                    detail,
                };
                writeln!(file, "{}", serde_json::to_string(&article)?)?;
                file.flush()?; // survive a crash mid-run
                ok += 1;
            }
            None => failed += 1,
        }

        sleep(Duration::from_millis(1000)).await; // be polite
    }
    Ok((ok, failed))
}
