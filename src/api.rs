use crate::cache::Cache;
use crate::epub::Book;
use crate::http::HttpClient;
use crate::range::{ranged_title, RangeRequest};

pub struct RoyalRoadApi {
    client: HttpClient,
}

impl RoyalRoadApi {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {
            client: HttpClient::new()?,
        })
    }

    pub async fn get_book(
        &self,
        id: u32,
        ignore_cache: bool,
        range: &RangeRequest,
    ) -> eyre::Result<Book> {
        tracing::info!("Fetching fiction page for {id}.");
        let mut book = Book::new(self.client.clone(), id).await?;
        let total = book.chapters.len();
        let resolved = range.resolve(
            &book
                .chapters
                .iter()
                .map(|chapter| chapter.id)
                .collect::<Vec<_>>(),
        )?;
        tracing::info!(
            "Chapter range: TOC {}-{} of {total}{}.",
            resolved.start + 1,
            resolved.end + 1,
            if resolved.end_is_latest {
                " (through latest)"
            } else {
                ""
            }
        );

        tracing::info!("Updating cover.");
        book.update_cover().await?;

        if !ignore_cache {
            if let Some(cached) = Cache::read_book(id)? {
                for chapter in book.chapters.iter_mut() {
                    if let Some(old) = cached
                        .chapters
                        .iter()
                        .find(|cached| cached.id == chapter.id)
                    {
                        if old.date == chapter.date && old.content.is_some() {
                            chapter.content = old.content.clone();
                            chapter.authors_note_start = old.authors_note_start.clone();
                            chapter.authors_note_end = old.authors_note_end.clone();
                        }
                    }
                }
            }
        }

        book.update_chapter_content(resolved.start, resolved.end)
            .await?;
        Cache::write_book(&book)?;

        let mut epub_book = book.clone();
        epub_book.chapters = book.chapters[resolved.start..=resolved.end].to_vec();
        if let Some(first) = epub_book.chapters.first() {
            epub_book.date_published = first.date.clone();
        }

        let unrestricted = resolved.start == 0 && resolved.end_is_latest;
        if unrestricted {
            epub_book.range_start_id = None;
            epub_book.range_end_id = None;
            epub_book.range_end_latest = false;
        } else {
            epub_book.title = ranged_title(&book.title, resolved.start + 1, resolved.end + 1);
            epub_book.range_start_id = epub_book.chapters.first().map(|chapter| chapter.id);
            epub_book.range_end_latest = resolved.end_is_latest;
            epub_book.range_end_id = if resolved.end_is_latest {
                None
            } else {
                epub_book.chapters.last().map(|chapter| chapter.id)
            };
        }

        Ok(epub_book)
    }
}
