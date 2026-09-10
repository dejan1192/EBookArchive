//! Library Genesis search backed by its HTML results table.

use std::collections::HashSet;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use tokio::io::AsyncWriteExt;

use super::{Book, Download, Format, Progress, SearchResults, Source, log_http_failure};

const SEARCH_URL: &str = "https://libgen.li/";
const DOWNLOAD_URL: &str = "https://libgen.li/get.php?md5=";
const MAX_DOWNLOAD_ATTEMPTS: usize = 5;

#[derive(Debug, Default)]
pub struct Libgen;

#[async_trait]
impl Source for Libgen {
    fn name(&self) -> &'static str {
        "libgen"
    }

    async fn search(
        &self,
        client: &Client,
        query: &str,
        page: usize,
        limit: usize,
    ) -> Result<SearchResults> {
        let page = page.max(1).to_string();
        let body = client
            .get(SEARCH_URL)
            .query(&[("req", query), ("page", page.as_str())])
            .send()
            .await
            .context("querying Libgen")?
            .error_for_status()?
            .text()
            .await
            .context("reading Libgen response")?;

        Ok(parse(&body, limit))
    }

    async fn fetch(
        &self,
        client: &Client,
        book: &Book,
        download: &Download,
        dest_dir: &Path,
        progress: Progress<'_>,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("creating {}", dest_dir.display()))?;
        let path = dest_dir.join(super::filename(book, download));
        let mut request_url = download.url.clone();

        for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
            let response = client
                .get(&request_url)
                .header(reqwest::header::ACCEPT, "application/octet-stream")
                .header(reqwest::header::REFERER, SEARCH_URL)
                // This CDN serves its HTML landing page for a plain GET, but
                // returns the complete attachment for an open-ended range.
                .header(reqwest::header::RANGE, "bytes=0-")
                .send()
                .await
                .with_context(|| format!("GET {}", download.url))?;
            if !response.status().is_success() {
                let retry = response.status().is_server_error() && attempt < MAX_DOWNLOAD_ATTEMPTS;
                let fallback_url = retry.then(|| alternate_cdn_url(response.url())).flatten();
                let error = log_http_failure(response, book, dest_dir).await;
                if retry {
                    if let Some(url) = fallback_url {
                        request_url = url;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64))
                        .await;
                    continue;
                }
                return Err(error);
            }
            let response_url = response.url().clone();
            let response_status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let attachment = response
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().contains("attachment"));

            if content_type.starts_with("text/html") || !attachment {
                if let Ok(body) = response.text().await
                    && let Some(keyed_url) = keyed_download_url(&body, &book.id)
                    && attempt < MAX_DOWNLOAD_ATTEMPTS
                {
                    request_url = keyed_url;
                    continue;
                }
                if attempt < MAX_DOWNLOAD_ATTEMPTS {
                    request_url = download.url.clone();
                    continue;
                }
                bail!(
                    "LibGen returned HTML instead of an ebook after {MAX_DOWNLOAD_ATTEMPTS} attempts ({response_status} from {response_url})"
                );
            }

            let content_length = response.content_length();
            let total = content_length.or(download.size);
            let mut file = tokio::fs::File::create(&path)
                .await
                .with_context(|| format!("creating {}", path.display()))?;
            let mut seen = 0u64;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("reading LibGen response body")?;
                seen += chunk.len() as u64;
                file.write_all(&chunk).await?;
                progress(seen, total);
            }
            file.flush().await?;

            if content_length.is_some_and(|expected| seen != expected) {
                if attempt < MAX_DOWNLOAD_ATTEMPTS {
                    continue;
                }
                bail!("incomplete LibGen download: received {seen} bytes, expected {total:?}");
            }
            return Ok(path);
        }

        bail!("LibGen download exhausted all retry attempts")
    }
}

fn keyed_download_url(body: &str, expected_hash: &str) -> Option<String> {
    let document = Html::parse_document(body);
    let links = Selector::parse("a[href]").expect("valid keyed download selector");
    let href = document
        .select(&links)
        .filter_map(|link| link.value().attr("href"))
        .find(|href| {
            href.starts_with("get.php?md5=")
                && href.contains("&key=")
                && href.contains(expected_hash)
        })?;
    Some(format!("https://libgen.li/{href}"))
}

fn alternate_cdn_url(url: &reqwest::Url) -> Option<String> {
    let alternate = match url.host_str()? {
        "cdn2.booksdl.lc" => "cdn3.booksdl.lc",
        "cdn3.booksdl.lc" => "cdn2.booksdl.lc",
        _ => return None,
    };
    let mut url = url.clone();
    url.set_host(Some(alternate)).ok()?;
    Some(url.to_string())
}

fn parse(body: &str, limit: usize) -> SearchResults {
    let document = Html::parse_document(body);
    let rows = Selector::parse("table#tablelibgen tbody tr").expect("valid row selector");
    let mut seen = HashSet::new();

    let books = document
        .select(&rows)
        .filter_map(book_from_row)
        .filter(|book| seen.insert(book.id.clone()))
        .take(limit)
        .collect();

    SearchResults {
        books,
        total_pages: total_pages(body, &document),
    }
}

fn book_from_row(row: ElementRef<'_>) -> Option<Book> {
    let cell_selector = Selector::parse("td").expect("valid cell selector");
    let edition_selector = Selector::parse("a[href^='edition.php']").expect("valid title selector");
    let link_selector = Selector::parse("a[href]").expect("valid link selector");
    let cells: Vec<_> = row.select(&cell_selector).collect();
    if cells.len() < 9 {
        return None;
    }

    let title = cells[0]
        .select(&edition_selector)
        .filter_map(direct_text_of)
        .max_by_key(String::len)?;
    let authors = text_of(cells[1]).into_iter().collect();
    let language = text_of(cells[4]);
    let size = text_of(cells[6]).and_then(|value| parse_size(&value));
    let format = text_of(cells[7]).and_then(|extension| format_from_extension(&extension))?;
    let hash = cells[8]
        .select(&link_selector)
        .filter_map(|link| link.value().attr("href"))
        .find_map(md5_from_href)?;

    Some(Book {
        source: "libgen",
        id: hash.clone(),
        title,
        authors,
        language,
        downloads: vec![Download {
            format,
            url: format!("{DOWNLOAD_URL}{hash}"),
            size,
        }],
    })
}

fn text_of(element: ElementRef<'_>) -> Option<String> {
    let text = element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn direct_text_of(element: ElementRef<'_>) -> Option<String> {
    let text = element
        .children()
        .filter_map(|child| child.value().as_text())
        .map(|text| text.text.as_ref())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn md5_from_href(href: &str) -> Option<String> {
    let (_, tail) = href.split_once("/md5/")?;
    let hash = tail.split(['/', '?', '#']).next()?;

    (hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
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
    let mut parts = value.split_whitespace();
    let amount: f64 = parts.next()?.replace(',', ".").parse().ok()?;
    let multiplier = match parts.next()?.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        _ => return None,
    };
    Some((amount * multiplier) as u64)
}

fn total_pages(body: &str, document: &Html) -> Option<usize> {
    let marker = "new Paginator(\"paginator_example_bottom\",";
    let from_script = body
        .split_once(marker)
        .and_then(|(_, tail)| tail.split(',').next())
        .and_then(|value| value.trim().parse().ok());
    if from_script.is_some() {
        return from_script;
    }

    let links =
        Selector::parse("#paginator_example_bottom a[href]").expect("valid paginator selector");
    document
        .select(&links)
        .filter_map(|link| link.value().attr("href"))
        .filter_map(page_from_href)
        .max()
}

fn page_from_href(href: &str) -> Option<usize> {
    href.split(['?', '&'])
        .find_map(|part| part.strip_prefix("page="))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "184c16eae281bf2ebea72eb6d7aa6f6d";

    const FIXTURE: &str = r#"
      <table id="tablelibgen"><tbody>
        <tr>
          <td><b>A series</b><br><a href="edition.php?id=1">The Book <i>Second edition</i></a></td>
          <td>Jane Doe</td><td>Publisher</td><td>2024</td><td>English</td><td>200</td>
          <td>2.5 MB</td><td>epub</td>
          <td>
            <a href="/ads.php?md5=184c16eae281bf2ebea72eb6d7aa6f6d">1</a>
            <a href="https://example.test/md5/184c16eae281bf2ebea72eb6d7aa6f6d?r=x">2</a>
          </td>
        </tr>
        <tr>
          <td><a href="edition.php?id=2">Duplicate file</a></td>
          <td>Someone Else</td><td></td><td></td><td>French</td><td></td><td>3 MB</td><td>pdf</td>
          <td><a href="/md5/184C16EAE281BF2EBEA72EB6D7AA6F6D">mirror</a></td>
        </tr>
        <tr><td>Malformed row</td></tr>
      </tbody></table>
      <div id="paginator_example_bottom"><a href="/?req=x&amp;page=7">7</a></div>
    "#;

    #[test]
    fn parses_metadata_and_removes_duplicate_hashes() {
        let results = parse(FIXTURE, 25);

        assert_eq!(results.books.len(), 1);
        assert_eq!(results.total_pages, Some(7));
        let book = &results.books[0];
        assert_eq!(book.id, HASH);
        assert_eq!(book.title, "The Book");
        assert_eq!(book.authors, vec!["Jane Doe"]);
        assert_eq!(book.language.as_deref(), Some("English"));
        assert_eq!(book.downloads[0].format, Format::Epub);
        assert_eq!(book.downloads[0].size, Some(2_500_000));
        assert_eq!(book.downloads[0].url, format!("{DOWNLOAD_URL}{HASH}"));
    }

    #[test]
    fn reads_total_pages_from_server_script() {
        let body =
            r#"<script>new Paginator("paginator_example_bottom", 80, 25, 1, "/?page=");</script>"#;
        let document = Html::parse_document(body);

        assert_eq!(total_pages(body, &document), Some(80));
    }

    #[test]
    fn applies_limit_after_deduplication() {
        assert!(parse(FIXTURE, 0).books.is_empty());
        assert_eq!(parse(FIXTURE, 1).books.len(), 1);
    }

    #[test]
    fn rejects_non_ebook_extensions() {
        assert_eq!(format_from_extension("bin"), None);
        assert_eq!(format_from_extension("rar"), None);
        assert_eq!(format_from_extension("cbz"), None);
        assert_eq!(format_from_extension("azw3"), Some(Format::Azw3));
        assert_eq!(format_from_extension("djvu"), Some(Format::Djvu));
    }

    #[test]
    fn extracts_keyed_download_for_the_expected_hash() {
        let body = format!(r#"<a href="get.php?md5={HASH}&amp;key=ABC123"><h2>GET</h2></a>"#);

        assert_eq!(
            keyed_download_url(&body, HASH).as_deref(),
            Some("https://libgen.li/get.php?md5=184c16eae281bf2ebea72eb6d7aa6f6d&key=ABC123")
        );
        assert_eq!(
            keyed_download_url(&body, "00000000000000000000000000000000"),
            None
        );
    }

    #[test]
    fn switches_between_libgen_cdn_hosts_without_changing_the_key() {
        let url = reqwest::Url::parse("https://cdn3.booksdl.lc/get.php?md5=abc&key=temporary-key")
            .unwrap();

        assert_eq!(
            alternate_cdn_url(&url).as_deref(),
            Some("https://cdn2.booksdl.lc/get.php?md5=abc&key=temporary-key")
        );
    }
}
