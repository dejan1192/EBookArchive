//! Serbian/Croatian/Bosnian ebooks from e-biblioteka.org's WordPress API.

use std::collections::HashSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::{Client, header::HeaderMap};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

use super::{Book, Download, Format, SearchResults, Source};

const API_URL: &str = "https://e-biblioteka.org/wp-json/wp/v2/posts";
const LANGUAGE: &str = "Serbian/Croatian/Bosnian";

#[derive(Debug, Default)]
pub struct EBiblioteka;

#[async_trait]
impl Source for EBiblioteka {
    fn name(&self) -> &'static str {
        "e-biblioteka"
    }

    async fn search(
        &self,
        client: &Client,
        query: &str,
        page: usize,
        limit: usize,
    ) -> Result<SearchResults> {
        let requested_page = page.max(1);
        let page = requested_page.to_string();
        let per_page = limit.clamp(1, 100).to_string();
        let response = client
            .get(API_URL)
            .query(&[
                ("search", query),
                ("page", page.as_str()),
                ("per_page", per_page.as_str()),
            ])
            .send()
            .await
            .context("querying e-biblioteka")?
            .error_for_status()?;
        let reported_pages = total_pages(response.headers());
        let posts: Vec<Post> = response
            .json()
            .await
            .context("decoding e-biblioteka response")?;

        let books = parse_posts(posts, limit);
        let books = add_sizes(client, books).await;
        // WordPress counts old posts that no longer expose a downloadable
        // file through the REST API. If such posts fill the tail page, do not
        // advertise that page as a usable book-results page.
        let total_pages = effective_total_pages(reported_pages, requested_page, books.is_empty());
        Ok(SearchResults { books, total_pages })
    }
}

#[derive(Deserialize)]
struct Post {
    id: u64,
    title: Rendered,
    content: Rendered,
}

#[derive(Deserialize)]
struct Rendered {
    rendered: String,
}

fn parse_posts(posts: Vec<Post>, limit: usize) -> Vec<Book> {
    let mut seen = HashSet::new();
    posts
        .into_iter()
        .flat_map(books_from_post)
        .filter(|book| seen.insert(book.downloads[0].url.clone()))
        .take(limit)
        .collect()
}

fn books_from_post(post: Post) -> Vec<Book> {
    let author = text_from_html(&post.title.rendered);
    let document = Html::parse_fragment(&post.content.rendered);
    let files = Selector::parse(".wp-block-file").expect("valid file selector");
    let links = Selector::parse("a[href]").expect("valid link selector");

    document
        .select(&files)
        .filter_map(|file| {
            let link = file.select(&links).next()?;
            let href = link.value().attr("href")?;
            let format = format_from_url(href)?;
            let url = upgrade_url(href);
            let title = text_of(link).or_else(|| title_from_url(&url))?;
            let id = format!("{}-{}", post.id, id_from_url(&url));

            Some(Book {
                source: "e-biblioteka",
                id,
                title,
                authors: author.clone().into_iter().collect(),
                language: Some(LANGUAGE.to_string()),
                downloads: vec![Download {
                    format,
                    url,
                    size: None,
                }],
            })
        })
        .collect()
}

async fn add_sizes(client: &Client, books: Vec<Book>) -> Vec<Book> {
    let mut completed = stream::iter(books.into_iter().enumerate().map(|(index, mut book)| {
        let client = client.clone();
        async move {
            if let Some(download) = book.downloads.first_mut()
                && let Ok(response) = client.head(&download.url).send().await
                && response.status().is_success()
            {
                download.size = response.content_length();
            }
            (index, book)
        }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, book)| book).collect()
}

fn total_pages(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("x-wp-totalpages")?
        .to_str()
        .ok()?
        .parse::<usize>()
        .ok()
        .map(|pages| pages.max(1))
}

fn effective_total_pages(
    reported: Option<usize>,
    requested_page: usize,
    books_empty: bool,
) -> Option<usize> {
    if books_empty && requested_page > 1 {
        return Some(
            reported
                .unwrap_or(requested_page - 1)
                .min(requested_page - 1)
                .max(1),
        );
    }
    reported
}

fn format_from_url(url: &str) -> Option<Format> {
    let extension = url.split(['?', '#']).next()?.rsplit('.').next()?;
    match extension.to_ascii_lowercase().as_str() {
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

fn text_of(element: ElementRef<'_>) -> Option<String> {
    let text = element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn text_from_html(value: &str) -> Option<String> {
    let document = Html::parse_fragment(value);
    let text = document
        .root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn upgrade_url(url: &str) -> String {
    url.strip_prefix("http://e-biblioteka.org/")
        .map(|path| format!("https://e-biblioteka.org/{path}"))
        .unwrap_or_else(|| url.to_string())
}

fn id_from_url(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .and_then(|url| url.rsplit('/').next())
        .and_then(|name| name.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or("book")
        .to_string()
}

fn title_from_url(url: &str) -> Option<String> {
    let title = id_from_url(url).replace(['-', '_'], " ");
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ebook_links_and_ignores_other_files() {
        let posts = vec![Post {
            id: 573,
            title: Rendered {
                rendered: "J.K. &amp; Rowling".to_string(),
            },
            content: Rendered {
                rendered: r#"
                    <div class="wp-block-file">
                      <a href="http://e-biblioteka.org/uploads/Hari-Poter.pdf">Hari Poter</a>
                      <a href="http://e-biblioteka.org/uploads/Hari-Poter.pdf" download>Download</a>
                    </div>
                    <div class="wp-block-file"><a href="/uploads/archive.zip">Archive</a></div>
                "#
                .to_string(),
            },
        }];

        let books = parse_posts(posts, 25);
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].title, "Hari Poter");
        assert_eq!(books[0].authors, vec!["J.K. & Rowling"]);
        assert_eq!(books[0].language.as_deref(), Some(LANGUAGE));
        assert_eq!(books[0].downloads[0].format, Format::Pdf);
        assert_eq!(
            books[0].downloads[0].url,
            "https://e-biblioteka.org/uploads/Hari-Poter.pdf"
        );
    }

    #[test]
    fn accepts_only_ebook_extensions() {
        assert_eq!(
            format_from_url("https://example.test/book.azw3"),
            Some(Format::Azw3)
        );
        assert_eq!(
            format_from_url("https://example.test/book.fb2?q=1"),
            Some(Format::Fb2)
        );
        assert_eq!(format_from_url("https://example.test/book.bin"), None);
        assert_eq!(format_from_url("https://example.test/book.zip"), None);
    }

    #[test]
    fn drops_a_reported_tail_page_without_downloadable_books() {
        assert_eq!(effective_total_pages(Some(4), 3, false), Some(4));
        assert_eq!(effective_total_pages(Some(4), 4, true), Some(3));
        assert_eq!(effective_total_pages(None, 4, true), Some(3));
    }
}
