//! Template for a [`Source`] backed by an HTML page instead of a JSON API.
//!
//! Not registered in [`super::all`] -- `BASE_URL` and the selectors below are
//! placeholders. Point it at a catalog you've checked (see the note on
//! robots.txt at the bottom) and add it there.
//!
//! The difference from a JSON source is only in how bytes become a [`Book`]:
//! `serde` derives a struct, here CSS selectors walk a document. Everything
//! else -- the trait, download, progress -- is unchanged.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};

use super::{Book, Download, Format, SearchResults, Source};

const BASE_URL: &str = "https://example.org/search";

/// Selectors are compiled once. `expect` is fine here: these are literals, so a
/// bad one is a bug that shows up on first use, not bad input at runtime.
static ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("li.book").expect("valid selector"));
static TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.title").expect("valid selector"));
static AUTHOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("span.author").expect("valid selector"));
static LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.download").expect("valid selector"));

pub struct HtmlCatalog;

#[async_trait]
impl Source for HtmlCatalog {
    fn name(&self) -> &'static str {
        "html-catalog"
    }

    async fn search(
        &self,
        client: &Client,
        query: &str,
        _page: usize,
        limit: usize,
    ) -> Result<SearchResults> {
        let body = client
            .get(BASE_URL)
            .query(&[("q", query)])
            .send()
            .await
            .context("fetching search page")?
            .error_for_status()?
            .text()
            .await
            .context("reading search page")?;

        // Parsing happens in a plain fn, and the parsed document never crosses
        // an `.await`. `scraper::Html` is !Send (it holds a `Cell`), and
        // async_trait futures must be Send -- holding one across an await point
        // fails to compile.
        Ok(SearchResults {
            books: parse(&body, limit),
            total_pages: None,
        })
    }
}

fn parse(body: &str, limit: usize) -> Vec<Book> {
    let document = Html::parse_document(body);

    document
        .select(&ROW)
        .take(limit)
        .filter_map(|row| book_from_row(row))
        .collect()
}

fn book_from_row(row: ElementRef<'_>) -> Option<Book> {
    // A missing title means the row isn't a result -- skip it rather than
    // inventing an empty book. HTML has no schema, so every field is Option.
    let title = text_of(row, &TITLE)?;

    let href = row.select(&LINK).next()?.value().attr("href")?;
    let url = absolute(href);
    let format = format_from_url(&url);

    Some(Book {
        source: "html-catalog",
        id: id_from_url(&url),
        title,
        authors: text_of(row, &AUTHOR).into_iter().collect(),
        language: None,
        downloads: vec![Download {
            format,
            url,
            size: None,
        }],
    })
}

/// Collapse a node's descendant text into one trimmed string.
fn text_of(row: ElementRef<'_>, selector: &Selector) -> Option<String> {
    let node = row.select(selector).next()?;
    let text: String = node
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn absolute(href: &str) -> String {
    match href.starts_with("http") {
        true => href.to_string(),
        false => format!("{}{href}", BASE_URL.trim_end_matches("/search")),
    }
}

fn format_from_url(url: &str) -> Format {
    match url.rsplit('.').next().unwrap_or("") {
        "epub" => Format::Epub,
        "pdf" => Format::Pdf,
        "txt" => Format::Txt,
        "mobi" => Format::Mobi,
        "torrent" => Format::Torrent,
        other => Format::Other(other.to_string()),
    }
}

fn id_from_url(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixture keeps the parser testable with no network and no live site.
    const FIXTURE: &str = r#"
        <html><body>
          <ol>
            <li class="book">
              <a class="title" href="/b/1">  A   Sample   Title </a>
              <span class="author">Some Author</span>
              <a class="download" href="/files/1.epub">get</a>
            </li>
            <li class="book">
              <a class="title" href="/b/2">Another Book</a>
              <span class="author">Second Author</span>
              <a class="download" href="https://cdn.example.org/2.pdf">get</a>
            </li>
            <li class="book"><span class="author">no title, skipped</span></li>
          </ol>
        </body></html>
    "#;

    #[test]
    fn parses_rows() {
        let books = parse(FIXTURE, 10);
        assert_eq!(books.len(), 2, "the row without a title is skipped");

        assert_eq!(books[0].title, "A Sample Title", "whitespace collapsed");
        assert_eq!(books[0].authors, vec!["Some Author"]);
        assert_eq!(books[0].downloads[0].format, Format::Epub);
        assert_eq!(
            books[0].downloads[0].url,
            "https://example.org/files/1.epub"
        );

        assert_eq!(books[1].downloads[0].format, Format::Pdf);
        assert_eq!(
            books[1].downloads[0].url, "https://cdn.example.org/2.pdf",
            "absolute hrefs are left alone"
        );
    }

    #[test]
    fn respects_limit() {
        assert_eq!(parse(FIXTURE, 1).len(), 1);
    }
}
