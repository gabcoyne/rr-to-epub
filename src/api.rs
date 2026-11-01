use crate::cache::Cache;
use crate::clean::contains_theft_marker;
use crate::epub::{Book, Chapter};
use crate::http::HttpClient;
use crate::range::{select_range, ChapterQuery};

pub struct RoyalRoadApi {
    http: HttpClient,
}

impl RoyalRoadApi {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {
            http: HttpClient::new()?,
        })
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    pub async fn get_book(
        &self,
        id: u32,
        source_url: Option<&str>,
        query: &ChapterQuery,
        ignore_cache: bool,
    ) -> eyre::Result<Book> {
        let mut book = Book::fetch(&self.http, id, source_url).await?;
        tracing::info!("Updating cover.");
        book.update_cover(&self.http).await?;

        if let Some(cached) = Cache::read_book(id)? {
            merge_cached_chapters(&mut book, &cached);
        }

        let selection = select_range(&book.chapters, query)?;
        let count = selection.end - selection.start + 1;
        tracing::info!(
            "Selected {count} chapters, TOC {}-{}: '{}' through '{}'",
            selection.start + 1,
            selection.end + 1,
            book.chapters[selection.start].title,
            book.chapters[selection.end].title
        );

        if ignore_cache {
            for index in selection.start..=selection.end {
                clear_chapter(&mut book.chapters[index]);
            }
            Cache::clear_download_progress(id)?;
        }

        book.download_range(&self.http, selection.start, selection.end, ignore_cache)
            .await?;
        Cache::write_book(&book)?;
        Cache::clear_download_progress(id)?;

        Ok(book.into_epub_slice(&selection, query))
    }
}

fn merge_cached_chapters(book: &mut Book, cached: &Book) {
    for chapter in &mut book.chapters {
        let Some(old) = cached.chapters.iter().find(|item| item.url == chapter.url) else {
            continue;
        };
        if old.date != chapter.date {
            continue;
        }
        let Some(content) = &old.content else {
            continue;
        };
        if contains_theft_marker(content) {
            tracing::info!(
                "Cached chapter '{}' still has anti-theft text; downloading it again",
                chapter.title
            );
            continue;
        }
        chapter.content = Some(content.clone());
        chapter.authors_note_start = old.authors_note_start.clone();
        chapter.authors_note_end = old.authors_note_end.clone();
    }
}

fn clear_chapter(chapter: &mut Chapter) {
    chapter.content = None;
    chapter.authors_note_start = None;
    chapter.authors_note_end = None;
}
