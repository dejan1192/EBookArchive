//! Project Gutenberg, via the Gutendex JSON API (<https://gutendex.com>).
//!
//! Public-domain texts, no API key, direct download links. Note the host 301s,
//! so the client must follow redirects (reqwest does by default).

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde::Deserialize;

use super::{Book, Download, Format, SearchResults, Source};

const BASE_URL: &str = "https://gutendex.com/books";
const PAGE_SIZE: usize = 32;

#[derive(Debug, Default)]
pub struct Gutendex;

#[async_trait]
impl Source for Gutendex {
    fn name(&self) -> &'static str {
        "gutenberg"
    }

    async fn search(
        &self,
        client: &Client,
        query: &str,
        page: usize,
        limit: usize,
    ) -> Result<SearchResults> {
        let response: SearchResponse = client
            .get(BASE_URL)
            .query(&[("search", query), ("page", &page.max(1).to_string())])
            .send()
            .await
            .context("querying gutendex")?
            .error_for_status()?
            .json()
            .await
            .context("decoding gutendex response")?;

        let total_pages = response.count.div_ceil(PAGE_SIZE).max(1);
        let books = response
            .results
            .into_iter()
            .take(limit)
            .map(Item::into_book)
            .collect();
        let books = add_sizes(client, books).await;

        Ok(SearchResults {
            books,
            total_pages: Some(total_pages),
        })
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    count: usize,
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    id: u64,
    title: String,
    #[serde(default)]
    authors: Vec<Author>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    formats: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Author {
    name: String,
}

impl Item {
    fn into_book(self) -> Book {
        let mut downloads: Vec<Download> = self
            .formats
            .into_iter()
            .filter(|(_, url)| !url.ends_with(".zip"))
            .filter_map(|(mime, url)| match Format::from_mime(&mime) {
                // Gutendex also lists cover images and RDF metadata; skip those.
                Format::Other(_) => None,
                format => Some(Download {
                    format,
                    url,
                    size: None,
                }),
            })
            .collect();

        // Stable order so the same book always presents its formats the same way.
        downloads.sort_by_key(|d| d.format.extension().to_string());

        Book {
            source: "gutenberg",
            id: self.id.to_string(),
            title: self.title,
            authors: self.authors.into_iter().map(|a| a.name).collect(),
            language: languages(self.languages),
            downloads,
        }
    }
}

async fn add_sizes(client: &Client, books: Vec<Book>) -> Vec<Book> {
    let mut completed = stream::iter(books.into_iter().enumerate().map(|(index, mut book)| {
        let client = client.clone();
        async move {
            let downloads = std::mem::take(&mut book.downloads);
            let mut sized = stream::iter(downloads.into_iter().enumerate().map(
                |(download_index, mut download)| {
                    let client = client.clone();
                    async move {
                        if let Ok(response) = client.head(&download.url).send().await
                            && response.status().is_success()
                        {
                            download.size = response.content_length();
                        }
                        (download_index, download)
                    }
                },
            ))
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;
            sized.sort_by_key(|(download_index, _)| *download_index);
            book.downloads = sized.into_iter().map(|(_, download)| download).collect();
            (index, book)
        }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, book)| book).collect()
}

fn languages(codes: Vec<String>) -> Option<String> {
    let names = codes
        .into_iter()
        .map(|code| language_name(&code).to_string())
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join(", "))
}

fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "fr" => "French",
        "de" => "German",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "ru" => "Russian",
        "pl" => "Polish",
        "cs" => "Czech",
        "sr" => "Serbian",
        "hr" => "Croatian",
        "bs" => "Bosnian",
        "zh" => "Chinese",
        "ja" => "Japanese",
        "la" => "Latin",
        "el" => "Greek",
        "fi" => "Finnish",
        "sv" => "Swedish",
        "da" => "Danish",
        "no" => "Norwegian",
        "hu" => "Hungarian",
        "ar" => "Arabic",
        "he" => "Hebrew",
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_languages_and_formats() {
        let book = Item {
            id: 11,
            title: "Alice".to_string(),
            authors: vec![Author {
                name: "Lewis Carroll".to_string(),
            }],
            languages: vec!["en".to_string(), "fr".to_string()],
            formats: HashMap::from([
                (
                    "application/epub+zip".to_string(),
                    "https://example.test/a.epub".to_string(),
                ),
                (
                    "image/jpeg".to_string(),
                    "https://example.test/a.jpg".to_string(),
                ),
            ]),
        }
        .into_book();

        assert_eq!(book.language.as_deref(), Some("English, French"));
        assert_eq!(book.downloads.len(), 1);
        assert_eq!(book.downloads[0].format, Format::Epub);
    }

    #[test]
    fn preserves_unknown_language_codes() {
        assert_eq!(languages(vec!["eo".to_string()]).as_deref(), Some("eo"));
        assert_eq!(languages(Vec::new()), None);
    }
}
