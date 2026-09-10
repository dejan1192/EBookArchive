//! Book catalogs the app can search.
//!
//! Adding a source means two things: implement [`Source`] for it, and add it to
//! [`all`]. Everything else -- searching, ranking, downloading, progress -- is
//! shared. A source only overrides [`Source::fetch`] if a plain HTTP GET isn't
//! how its files come down (a torrent, say).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use tokio::io::AsyncWriteExt;

pub mod annas_archive;
pub mod e_biblioteka;
pub mod gutendex;
pub mod html_catalog;
pub mod libgen;

/// Every source we know how to search.
pub fn all() -> Vec<std::sync::Arc<dyn Source>> {
    vec![
        std::sync::Arc::new(gutendex::Gutendex),
        std::sync::Arc::new(libgen::Libgen),
        std::sync::Arc::new(e_biblioteka::EBiblioteka),
        std::sync::Arc::new(annas_archive::AnnasArchive::default()),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    Epub,
    Pdf,
    Txt,
    Html,
    Mobi,
    Azw3,
    Djvu,
    Fb2,
    Torrent,
    Other(String),
}

impl Format {
    pub fn extension(&self) -> &str {
        match self {
            Self::Epub => "epub",
            Self::Pdf => "pdf",
            Self::Txt => "txt",
            Self::Html => "html",
            Self::Mobi => "mobi",
            Self::Azw3 => "azw3",
            Self::Djvu => "djvu",
            Self::Fb2 => "fb2",
            Self::Torrent => "torrent",
            Self::Other(_) => "bin",
        }
    }

    /// Map a Content-Type to a format, ignoring any `; charset=...` suffix.
    pub fn from_mime(mime: &str) -> Self {
        match mime.split(';').next().unwrap_or("").trim() {
            "application/epub+zip" => Self::Epub,
            "application/pdf" => Self::Pdf,
            "text/plain" => Self::Txt,
            "text/html" => Self::Html,
            "application/x-mobipocket-ebook" => Self::Mobi,
            "application/x-bittorrent" => Self::Torrent,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(s) => write!(f, "{s}"),
            _ => write!(f, "{}", self.extension()),
        }
    }
}

/// One downloadable file belonging to a book.
#[derive(Debug, Clone)]
pub struct Download {
    pub format: Format,
    pub url: String,
    /// Only some sources report this up front.
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Book {
    pub source: &'static str,
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub language: Option<String>,
    pub downloads: Vec<Download>,
}

impl Book {
    pub fn author_line(&self) -> String {
        if self.authors.is_empty() {
            "Unknown".to_string()
        } else {
            self.authors.join(", ")
        }
    }

    /// First available download matching the caller's format preference order.
    pub fn preferred(&self, prefer: &[Format]) -> Option<&Download> {
        prefer
            .iter()
            .find_map(|want| self.downloads.iter().find(|d| &d.format == want))
            .or_else(|| self.downloads.first())
    }

    pub fn formats(&self) -> Vec<String> {
        self.downloads
            .iter()
            .map(|d| d.format.to_string())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub books: Vec<Book>,
    pub total_pages: Option<usize>,
}

/// Called with (bytes so far, total if known). Cheap and non-async on purpose,
/// so the TUI can just push into an mpsc channel from here.
pub type Progress<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;

    async fn search(
        &self,
        client: &Client,
        query: &str,
        page: usize,
        limit: usize,
    ) -> Result<SearchResults>;

    /// Release persistent resources such as a source-owned browser session.
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Pull one file to disk. The default is a streaming HTTP GET, which is
    /// what every direct-download catalog needs.
    async fn fetch(
        &self,
        client: &Client,
        book: &Book,
        download: &Download,
        dest_dir: &Path,
        progress: Progress<'_>,
    ) -> Result<PathBuf> {
        http_fetch(client, book, download, dest_dir, progress).await
    }
}

/// Stream a URL to `dest_dir`, reporting progress as it goes.
pub async fn http_fetch(
    client: &Client,
    book: &Book,
    download: &Download,
    dest_dir: &Path,
    progress: Progress<'_>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .with_context(|| format!("creating {}", dest_dir.display()))?;

    let mut successful = None;
    for attempt in 1..=3 {
        let response = client
            .get(&download.url)
            .send()
            .await
            .with_context(|| format!("GET {}", download.url))?;
        if response.status().is_success() {
            successful = Some(response);
            break;
        }
        let retry = response.status().is_server_error() && attempt < 3;
        let error = log_http_failure(response, book, dest_dir).await;
        if !retry {
            return Err(error);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300 * attempt)).await;
    }
    let response = successful.expect("three-attempt loop returns on its final HTTP error");

    let total = response.content_length().or(download.size);
    let path = dest_dir.join(filename(book, download));
    let mut file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;

    let mut seen = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body")?;
        seen += chunk.len() as u64;
        file.write_all(&chunk).await?;
        progress(seen, total);
    }
    file.flush().await?;

    Ok(path)
}

/// Preserve enough of an unsuccessful download response to diagnose an
/// intermittent upstream failure without overwriting earlier incidents.
pub(super) async fn log_http_failure(
    response: reqwest::Response,
    book: &Book,
    dest_dir: &Path,
) -> anyhow::Error {
    let status = response.status();
    let url = response.url().clone();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while body.len() < 4096 {
        let Some(chunk) = stream.next().await else {
            break;
        };
        match chunk {
            Ok(chunk) => {
                let remaining = 4096 - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Err(_) => break,
        }
    }
    let preview = String::from_utf8_lossy(&body).replace(['\r', '\n'], " ");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let selected_headers = [
        "content-type",
        "server",
        "cf-ray",
        "retry-after",
        "location",
    ]
    .into_iter()
    .filter_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| format!("{name}={value}"))
    })
    .collect::<Vec<_>>()
    .join("; ");
    let record = format!(
        "time_unix={timestamp}\nsource={}\nbook_id={}\ntitle={}\nstatus={}\nurl={}\nheaders={}\nbody_preview={}\n---\n",
        book.source,
        book.id,
        book.title.replace(['\r', '\n'], " "),
        status.as_u16(),
        url,
        selected_headers,
        preview,
    );
    let log_path = dest_dir.join("download-errors.log");
    let logged = async {
        tokio::fs::create_dir_all(dest_dir).await?;
        let mut log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await?;
        log.write_all(record.as_bytes()).await?;
        log.flush().await
    }
    .await;

    match logged {
        Ok(()) => anyhow::anyhow!(
            "download returned HTTP {}; details saved to {}",
            status.as_u16(),
            log_path.display()
        ),
        Err(error) => anyhow::anyhow!(
            "download returned HTTP {}; could not write {}: {error}",
            status.as_u16(),
            log_path.display()
        ),
    }
}

pub(super) fn filename(book: &Book, download: &Download) -> String {
    let title: String = book
        .title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let title = title.trim().chars().take(80).collect::<String>();
    format!(
        "{title} [{}-{}].{}",
        book.source,
        book.id,
        download.format.extension()
    )
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn retries_server_errors_and_appends_diagnostic_log() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let _ = socket.read(&mut request);
                socket
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops",
                    )
                    .unwrap();
            }
        });
        let directory = std::env::temp_dir().join(format!(
            "tui-download-log-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let book = Book {
            source: "test-source",
            id: "book-42".to_string(),
            title: "Intermittent book".to_string(),
            authors: Vec::new(),
            language: None,
            downloads: Vec::new(),
        };
        let download = Download {
            format: Format::Pdf,
            url: format!("http://{address}/book.pdf"),
            size: None,
        };

        let error = http_fetch(&Client::new(), &book, &download, &directory, &|_, _| {})
            .await
            .unwrap_err();
        server.join().unwrap();

        let log = std::fs::read_to_string(directory.join("download-errors.log")).unwrap();
        assert!(error.to_string().contains("HTTP 500"));
        assert_eq!(log.matches("status=500").count(), 3);
        assert!(log.contains("source=test-source"));
        assert!(log.contains("book_id=book-42"));
        assert!(log.contains("body_preview=oops"));
        let _ = std::fs::remove_dir_all(directory);
    }
}
