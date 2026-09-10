//! Exercise the source layer without the TUI.
//!
//!   cargo run --example search -- dostoevsky
//!   cargo run --example search -- dostoevsky --get 1

use std::path::Path;

use anyhow::{Result, bail};
use tui::source::{self, Format};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut get = None;
    let mut source_filter = None;
    let mut page = 1;
    let mut limit = 25;
    let mut query = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--get" => {
                index += 1;
                get = args
                    .get(index)
                    .and_then(|value| value.parse::<usize>().ok());
            }
            "--source" => {
                index += 1;
                source_filter = args.get(index).cloned();
            }
            "--page" => {
                index += 1;
                page = args
                    .get(index)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
            }
            "--limit" => {
                index += 1;
                limit = args
                    .get(index)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(25)
                    .max(1);
            }
            value => query.push(value),
        }
        index += 1;
    }
    if query.is_empty() {
        bail!("usage: search <query> [--source NAME] [--page N] [--limit N] [--get N]");
    }
    let query = query.join(" ");

    let client = reqwest::Client::builder()
        .user_agent("tui-book-search/0.1")
        .build()?;

    let mut books = Vec::new();
    for src in source::all().into_iter().filter(|source| {
        source_filter
            .as_deref()
            .is_none_or(|name| source.name() == name)
    }) {
        let found = src.search(&client, &query, page, limit).await?;
        println!(
            "{}: {} result(s), page {}{}",
            src.name(),
            found.books.len(),
            page,
            found
                .total_pages
                .map(|total| format!("/{total}"))
                .unwrap_or_default()
        );
        books.extend(found.books.into_iter().map(|b| (src.name(), b)));
        src.close().await?;
    }

    for (i, (_, book)) in books.iter().enumerate() {
        println!(
            "{:>2}. {}\n    {} [{}] {}",
            i + 1,
            book.title,
            book.author_line(),
            book.source,
            book.formats().join(", ")
        );
    }

    let Some(n) = get else { return Ok(()) };
    let Some((_, book)) = books.get(n - 1) else {
        bail!("no result {n}");
    };

    let prefer = [Format::Epub, Format::Txt, Format::Pdf];
    let Some(download) = book.preferred(&prefer) else {
        bail!("no downloadable file for {}", book.title);
    };

    println!("\ndownloading {} as {}", book.title, download.format);
    let src = source::all()
        .into_iter()
        .find(|s| s.name() == book.source)
        .expect("source that produced the book");

    let path = src
        .fetch(
            &client,
            book,
            download,
            Path::new("downloads"),
            &|seen, total| match total {
                Some(t) => print!("\r  {seen}/{t} bytes"),
                None => print!("\r  {seen} bytes"),
            },
        )
        .await?;

    println!("\nsaved to {}", path.display());
    Ok(())
}
