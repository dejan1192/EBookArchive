//! Anna's Archive search through a persistent headless Chromium session.
//!
//! The public site protects search and partner-link pages with DDoS-Guard, so
//! these two HTML pages are loaded in Chromium. The actual ebook is still
//! streamed by reqwest after Chromium resolves its temporary partner URL.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use thirtyfour::manager::BrowserKind;
use thirtyfour::prelude::*;
use tokio::sync::Mutex;

use super::{
    Book, Download, Format, Progress, SearchResults, Source, http_fetch, log_download_diagnostic,
};

const BASE_URL: &str = "https://annas-archive.gl";
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(35);
const PARTNER_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Default)]
pub struct AnnasArchive {
    driver: Mutex<Option<WebDriver>>,
}

#[async_trait]
impl Source for AnnasArchive {
    fn name(&self) -> &'static str {
        "annas-archive"
    }

    async fn search(
        &self,
        _client: &Client,
        query: &str,
        page: usize,
        limit: usize,
    ) -> Result<SearchResults> {
        let url = reqwest::Url::parse_with_params(
            &format!("{BASE_URL}/search"),
            &[("q", query), ("page", &page.max(1).to_string())],
        )?;
        let body = self.load_html(url.as_str()).await?;
        parse_search(&body, limit, page.max(1))
    }

    async fn close(&self) -> Result<()> {
        if let Some(driver) = self.driver.lock().await.take() {
            driver
                .quit()
                .await
                .context("closing Anna's Archive browser")?;
        }
        Ok(())
    }

    async fn fetch(
        &self,
        client: &Client,
        book: &Book,
        download: &Download,
        dest_dir: &Path,
        progress: Progress<'_>,
    ) -> Result<PathBuf> {
        let direct_url = self.resolve_download_url(book, download, dest_dir).await?;
        let direct = Download {
            format: download.format.clone(),
            url: direct_url,
            size: download.size,
        };
        http_fetch(client, book, &direct, dest_dir, progress).await
    }
}

impl AnnasArchive {
    async fn load_html(&self, url: &str) -> Result<String> {
        let mut slot = self.driver.lock().await;
        navigate(&mut slot, url).await?;
        slot.as_ref()
            .expect("driver initialized")
            .source()
            .await
            .context("reading Anna's Archive page")
    }

    /// Keep the browser locked until the dynamic partner link appears. This
    /// also prevents simultaneous downloads from navigating the shared tab
    /// away from one another midway through partner-link resolution.
    async fn resolve_download_url(
        &self,
        book: &Book,
        download: &Download,
        dest_dir: &Path,
    ) -> Result<String> {
        let mut slot = self.driver.lock().await;
        navigate(&mut slot, &download.url).await?;
        let driver = slot.as_ref().expect("driver initialized");
        let started = Instant::now();

        loop {
            let body = driver
                .source()
                .await
                .context("reading Anna's Archive partner page")?;
            if let Some(url) = direct_download_url(&body, &download.format) {
                return Ok(url);
            }
            if started.elapsed() >= PARTNER_TIMEOUT {
                let title = driver.title().await.unwrap_or_default();
                let details = format!(
                    "Anna's Archive did not provide a partner download URL within 25 seconds; page_title={title}; html={body}"
                );
                return Err(log_download_diagnostic(
                    book,
                    dest_dir,
                    "partner-link",
                    &download.url,
                    &details,
                )
                .await);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn navigate(slot: &mut Option<WebDriver>, url: &str) -> Result<()> {
    if slot.is_none() {
        *slot = Some(start_driver().await?);
    }

    let driver = slot.as_ref().expect("driver initialized");
    if let Err(error) = driver.goto(url).await {
        // A browser process may have disappeared between searches. Rebuild
        // it once, while retaining the same on-disk profile.
        *slot = None;
        let replacement = start_driver().await.context(error.to_string())?;
        replacement.goto(url).await?;
        *slot = Some(replacement);
    }
    let driver = slot.as_ref().expect("driver initialized");

    let started = Instant::now();
    loop {
        let title = driver.title().await.unwrap_or_default();
        if !title.contains("DDoS-Guard") && !title.contains("Just a moment") {
            break;
        }
        if started.elapsed() >= CHALLENGE_TIMEOUT {
            bail!("browser verification did not finish within 35 seconds");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

async fn start_driver() -> Result<WebDriver> {
    let profile = browser_profile_dir();
    tokio::fs::create_dir_all(&profile)
        .await
        .with_context(|| format!("creating browser profile {}", profile.display()))?;
    clear_stale_profile_lock(&profile)?;

    let mut caps = DesiredCapabilities::chrome();
    if Path::new("/usr/bin/chromium").exists() {
        caps.set_binary("/usr/bin/chromium")?;
    }
    for argument in [
        "--headless=new",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--disable-blink-features=AutomationControlled",
        "--window-size=1920,1080",
        "--force-device-scale-factor=1",
    ] {
        caps.add_arg(argument)?;
    }
    caps.add_arg(&format!("--user-data-dir={}", profile.display()))?;
    caps.set_browser_option("excludeSwitches", serde_json::json!(["enable-automation"]))?;
    caps.set_browser_option("useAutomationExtension", serde_json::json!(false))?;

    let driver = if Path::new("/usr/bin/chromedriver").exists() {
        WebDriver::managed(caps)
            .driver_binary(BrowserKind::Chrome, "/usr/bin/chromedriver")
            .await?
    } else {
        WebDriver::managed(caps).await?
    };

    let cdp = driver.cdp();
    cdp.send_raw(
        "Page.addScriptToEvaluateOnNewDocument",
        serde_json::json!({
            "source": "Object.defineProperty(navigator, 'webdriver', {get: () => undefined})"
        }),
    )
    .await?;
    cdp.send_raw(
        "Emulation.setDeviceMetricsOverride",
        serde_json::json!({
            "width": 1920,
            "height": 947,
            "deviceScaleFactor": 1,
            "mobile": false,
            "screenWidth": 1920,
            "screenHeight": 1080,
            "positionX": 0,
            "positionY": 0
        }),
    )
    .await?;
    let browser_version = cdp
        .send_raw("Browser.getVersion", serde_json::json!({}))
        .await?;
    if let Some(user_agent) = browser_version
        .get("userAgent")
        .and_then(|value| value.as_str())
    {
        cdp.send_raw(
            "Emulation.setUserAgentOverride",
            serde_json::json!({
                "userAgent": user_agent.replace("HeadlessChrome/", "Chrome/"),
                "platform": "Linux x86_64"
            }),
        )
        .await?;
    }

    Ok(driver)
}

/// Chromium can leave its singleton symlinks behind after an unclean exit.
/// Remove them only when their encoded process id no longer exists, otherwise
/// a second app instance could corrupt an active profile.
fn clear_stale_profile_lock(profile: &Path) -> Result<()> {
    #[cfg(unix)]
    let profile_in_use = std::fs::read_link(profile.join("SingletonSocket"))
        .ok()
        .is_some_and(|socket| std::os::unix::net::UnixStream::connect(socket).is_ok());
    #[cfg(not(unix))]
    let profile_in_use = true;

    if profile_in_use {
        return Ok(());
    }
    for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let path = profile.join(name);
        if path.symlink_metadata().is_ok() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale Chromium lock {}", path.display()))?;
        }
    }
    Ok(())
}

fn browser_profile_dir() -> PathBuf {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(cache).join("tui-book-search/annas-chromium");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/tui-book-search/annas-chromium");
    }
    std::env::temp_dir().join("tui-book-search/annas-chromium")
}

fn parse_search(body: &str, limit: usize, page: usize) -> Result<SearchResults> {
    let document = Html::parse_document(body);
    if limit == 0 {
        return Ok(SearchResults {
            books: Vec::new(),
            total_pages: pagination_end(&document, page, true),
        });
    }
    let result_links = Selector::parse("a.js-vim-focus[href^='/md5/']").unwrap();
    let author_links = Selector::parse("a[href^='/search?q=']").unwrap();
    let metadata_blocks = Selector::parse("div.text-gray-800").unwrap();
    let mut seen = HashSet::new();
    let mut books = Vec::new();

    for title_link in document.select(&result_links) {
        let Some(hash) = title_link
            .value()
            .attr("href")
            .and_then(|href| href.strip_prefix("/md5/"))
            .filter(|hash| hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        if !seen.insert(hash.clone()) {
            continue;
        }

        let Some(container) = result_container(title_link) else {
            continue;
        };
        let title = normalized_text(title_link);
        let authors = container
            .select(&author_links)
            .next()
            .map(normalized_text)
            .filter(|value| !value.is_empty())
            .into_iter()
            .collect();
        let Some(metadata) = container
            .select(&metadata_blocks)
            .next()
            .map(normalized_text)
        else {
            continue;
        };
        let Some((language, format, size)) = parse_metadata(&metadata) else {
            continue;
        };

        books.push(Book {
            source: "annas-archive",
            id: hash.clone(),
            title,
            authors,
            language,
            downloads: vec![Download {
                format,
                url: format!("{BASE_URL}/slow_download/{hash}/0/0"),
                size,
            }],
        });
        if books.len() == limit {
            break;
        }
    }

    if books.is_empty() && body.contains("DDoS-Guard") {
        bail!("Anna's Archive browser verification is still active");
    }

    Ok(SearchResults {
        total_pages: pagination_end(&document, page, books.is_empty()),
        books,
    })
}

fn result_container(link: ElementRef<'_>) -> Option<ElementRef<'_>> {
    link.ancestors()
        .filter_map(ElementRef::wrap)
        .find(|element| {
            element.value().name() == "div"
                && element.value().attr("class").is_some_and(|classes| {
                    classes.split_whitespace().any(|class| class == "border-b")
                })
        })
}

fn parse_metadata(value: &str) -> Option<(Option<String>, Format, Option<u64>)> {
    let mut parts = value.split('·').map(str::trim);
    let language = parts
        .next()
        .map(|value| value.split(" [").next().unwrap_or(value).trim().to_string())
        .filter(|value| !value.is_empty());
    let format = format_from_extension(parts.next()?)?;
    let size = parts.next().and_then(parse_size);
    Some((language, format, size))
}

fn format_from_extension(extension: &str) -> Option<Format> {
    match extension.trim().to_ascii_lowercase().as_str() {
        "epub" => Some(Format::Epub),
        "pdf" => Some(Format::Pdf),
        "txt" => Some(Format::Txt),
        "html" | "htm" => Some(Format::Html),
        "mobi" => Some(Format::Mobi),
        "azw3" => Some(Format::Azw3),
        "djvu" => Some(Format::Djvu),
        "fb2" => Some(Format::Fb2),
        _ => None,
    }
}

fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    for (suffix, multiplier) in [
        ("gb", 1_000_000_000_f64),
        ("mb", 1_000_000_f64),
        ("kb", 1_000_f64),
        ("b", 1_f64),
    ] {
        if let Some(amount) = value.strip_suffix(suffix) {
            return amount
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .ok()
                .map(|n| (n * multiplier) as u64);
        }
    }
    None
}

fn pagination_end(document: &Html, page: usize, books_empty: bool) -> Option<usize> {
    let links = Selector::parse("nav[aria-label='Pagination'] a[href*='page=']").unwrap();
    let has_next = document
        .select(&links)
        .filter_map(|link| link.value().attr("href"))
        .filter_map(page_from_href)
        .any(|linked_page| linked_page == page + 1);
    match (books_empty, has_next) {
        (true, _) if page > 1 => Some(page - 1),
        (_, true) => None,
        _ => Some(page.max(1)),
    }
}

fn page_from_href(href: &str) -> Option<usize> {
    href.split(['?', '&'])
        .find_map(|part| part.strip_prefix("page="))?
        .parse()
        .ok()
}

fn direct_download_url(body: &str, expected_format: &Format) -> Option<String> {
    let document = Html::parse_document(body);
    let links = Selector::parse("a[href^='https://']").unwrap();
    document.select(&links).find_map(|link| {
        let href = link.value().attr("href")?;
        let path = reqwest::Url::parse(href).ok()?.path().to_ascii_lowercase();
        path.ends_with(&format!(".{}", expected_format.extension()))
            .then(|| href.to_string())
    })
}

fn normalized_text(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "19ac63986ba813dd3500ae1d00a9cb18";

    #[test]
    fn parses_search_metadata_and_pages() {
        let body = format!(
            r#"
            <div class="flex pt-3 pb-3 border-b">
              <div>
                <a href="/md5/{HASH}" class="js-vim-focus">Marsovac</a>
                <a href="/search?q=Andy+Weir">Andy Weir</a>
              </div>
              <div class="text-gray-800">Serbian [sr] · EPUB · 0.4MB · 2013</div>
            </div>
            <nav aria-label="Pagination">
              <a href="/search?q=x&amp;page=2">2</a>
              <a href="/search?q=x&amp;page=10">10</a>
            </nav>
            "#
        );
        let results = parse_search(&body, 25, 1).unwrap();
        assert_eq!(results.total_pages, None);
        assert_eq!(results.books.len(), 1);
        let book = &results.books[0];
        assert_eq!(book.id, HASH);
        assert_eq!(book.title, "Marsovac");
        assert_eq!(book.authors, vec!["Andy Weir"]);
        assert_eq!(book.language.as_deref(), Some("Serbian"));
        assert_eq!(book.downloads[0].format, Format::Epub);
        assert_eq!(book.downloads[0].size, Some(400_000));
    }

    #[test]
    fn extracts_partner_url_for_expected_format() {
        let body = r#"
          <a href="https://example.test/ad.html">ad</a>
          <a href="https://partner.test/files/Marsovac.epub?token=abc">Download now</a>
        "#;
        assert_eq!(
            direct_download_url(body, &Format::Epub).as_deref(),
            Some("https://partner.test/files/Marsovac.epub?token=abc")
        );
        assert_eq!(direct_download_url(body, &Format::Pdf), None);
    }

    #[test]
    fn rejects_non_ebook_formats() {
        assert_eq!(format_from_extension("bin"), None);
        assert_eq!(format_from_extension("zip"), None);
        assert_eq!(format_from_extension("PDF"), Some(Format::Pdf));
    }

    #[test]
    fn only_reports_an_exact_total_at_the_end_of_pagination() {
        let middle = Html::parse_document(
            r#"<nav aria-label="Pagination"><a href="/search?q=x&amp;page=3">3</a></nav>"#,
        );
        assert_eq!(pagination_end(&middle, 2, false), None);

        let end = Html::parse_document(
            r#"<nav aria-label="Pagination"><a href="/search?q=x&amp;page=3">3</a></nav>"#,
        );
        assert_eq!(pagination_end(&end, 3, false), Some(3));
        assert_eq!(pagination_end(&end, 4, true), Some(3));
    }
}
