use crate::cache::{Cache, DownloadProgress};
use crate::clean::clean_chapter_html;
use crate::http::HttpClient;
use crate::range::{ChapterQuery, Selection};
use crate::xml_ext::{write_elements, xml_text};
use eyre::{bail, WrapErr};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::io::Reader as ImageReader;
use rand::Rng;
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use webp::Decoder;
use xml::writer::XmlEvent;
use xml::EmitterConfig;

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Book {
    pub id: u32,
    pub title: String,
    pub author: String,
    pub description: String,
    pub date_published: String,
    pub cover_url: String,
    pub cover: Option<Vec<u8>>,
    pub chapters: Vec<Chapter>,
    /// Fiction page that produced this book. Prefer the slug URL; the bare id URL is challenged more often.
    #[serde(default)]
    pub source_url: String,

    #[serde(skip)]
    pub epub_title: Option<String>,
    #[serde(skip)]
    pub range_label: Option<String>,
    #[serde(skip)]
    pub range_from: Option<String>,
    #[serde(skip)]
    pub range_to: Option<String>,
    #[serde(skip)]
    pub range_after: Option<String>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub id: u32,
    pub date: String,
    pub slug: String,
    pub title: String,
    pub url: String,
    pub content: Option<String>,
    pub authors_note_start: Option<String>,
    pub authors_note_end: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBook {
    pub id: u32,
    pub source_url: Option<String>,
    pub range_from: Option<String>,
    pub range_to: Option<String>,
    pub range_after: Option<String>,
}

impl StoredBook {
    pub fn read(path: impl AsRef<Path>) -> eyre::Result<Option<Self>> {
        let mut reader = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        let mut content_opf = reader.by_name("OEBPS/content.opf")?;
        let mut contents = String::new();
        content_opf.read_to_string(&mut contents)?;
        Ok(stored_book_from_opf(&contents))
    }
}

impl Book {
    pub async fn fetch(
        client: &HttpClient,
        id: u32,
        source_url: Option<&str>,
    ) -> eyre::Result<Self> {
        let page_url = fiction_page_url(id, source_url);
        tracing::info!("Fetching fiction page {page_url}");
        let response = client.get_text(&page_url).await?;
        if !response.contains("window.chapters") {
            bail!("Fiction page did not include a chapter list ({page_url})");
        }

        let parsed = Html::parse_document(&response);
        let title = normalized_text(
            parsed
                .select(&Selector::parse("h1").unwrap())
                .next()
                .ok_or_else(|| eyre::eyre!("No title found"))?,
        );
        let author = normalized_text(
            parsed
                .select(&Selector::parse("h4 a").unwrap())
                .next()
                .ok_or_else(|| eyre::eyre!("No author found"))?,
        );
        let description = parsed
            .select(&Selector::parse(".description > .hidden-content").unwrap())
            .next()
            .ok_or_else(|| eyre::eyre!("No description found"))?
            .inner_html();
        if title.is_empty() {
            bail!("No title found");
        }

        let cover_url = extract_js_string(&response, "window.fictionCover =")
            .or_else(|| {
                parsed
                    .select(&Selector::parse("meta[property='og:image']").unwrap())
                    .next()
                    .and_then(|el| el.value().attr("content"))
                    .map(str::to_string)
            })
            .ok_or_else(|| eyre::eyre!("No cover found"))?;
        let chapters_json = extract_js_json(&response, "window.chapters =")?;
        let chapters: Vec<Chapter> = serde_json::from_str(chapters_json)
            .wrap_err("failed to parse the Royal Road chapter list")?;
        if chapters.is_empty() {
            bail!("Fiction has no chapters");
        }
        tracing::info!("Found {} chapters in '{title}'", chapters.len());

        Ok(Self {
            id,
            cover_url,
            cover: None,
            title,
            author,
            description,
            date_published: chapters
                .first()
                .map(|chapter| chapter.date.clone())
                .unwrap_or_default(),
            chapters,
            source_url: page_url,
            ..Self::default()
        })
    }

    pub async fn update_cover(&mut self, client: &HttpClient) -> eyre::Result<()> {
        let candidates = cover_candidates(&self.cover_url);
        let mut best: Option<CoverChoice> = None;
        for (index, url) in candidates.iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let bytes = match client.get_bytes(url).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::debug!("Cover candidate {url} failed: {err}");
                    continue;
                }
            };
            let Ok(image) = prepare_cover_bytes(&bytes) else {
                tracing::debug!("Cover candidate {url} was not an image");
                continue;
            };
            let (width, height) = image_dimensions(&image).unwrap_or((0, 0));
            let area = u128::from(width) * u128::from(height);
            let replace = match &best {
                None => true,
                Some(current) => {
                    area > current.area
                        || (area == current.area && image.len() > current.bytes.len())
                }
            };
            if replace {
                tracing::info!(
                    "Cover candidate {url} is {width}x{height} ({} bytes)",
                    image.len()
                );
                best = Some(CoverChoice { area, bytes: image });
            }
        }
        let Some(best) = best else {
            bail!("No cover image could be downloaded from {}", self.cover_url);
        };
        tracing::info!("Using cover image ({} bytes)", best.bytes.len());
        self.cover = Some(best.bytes);
        Ok(())
    }

    pub async fn download_range(
        &mut self,
        client: &HttpClient,
        start: usize,
        end: usize,
        ignore_cache: bool,
    ) -> eyre::Result<()> {
        let ids: Vec<u32> = self.chapters[start..=end]
            .iter()
            .map(|chapter| chapter.id)
            .collect();
        let total = ids.len();
        let mut resume = if ignore_cache {
            0
        } else {
            match Cache::read_download_progress(self.id)? {
                Some(DownloadProgress::Scoped { next, ids: saved }) if saved == ids => {
                    next.min(total)
                }
                Some(DownloadProgress::Legacy(next))
                    if start == 0 && end + 1 == self.chapters.len() =>
                {
                    next.min(total)
                }
                _ => 0,
            }
        };
        if let Some(missing) =
            (0..total).find(|offset| self.chapters[start + offset].content.is_none())
        {
            if missing < resume {
                tracing::warn!(
                    "Checkpoint was at chapter {} but chapter {} has no content; resuming there",
                    resume + 1,
                    missing + 1
                );
                resume = missing;
            }
        }
        let missing = (start..=end)
            .filter(|index| self.chapters[*index].content.is_none())
            .count();
        if missing == 0 {
            tracing::info!("All {total} chapters found in cache.");
        } else if resume > 0 {
            tracing::info!("Resuming download from chapter {} of {total}", resume + 1);
        } else {
            tracing::info!("Downloading {missing} chapters ({total} in range).");
        }

        for offset in resume..total {
            let index = start + offset;
            let already = self.chapters[index].content.is_some();
            if !already {
                let title = self.chapters[index].title.clone();
                let url = chapter_page_url(&self.chapters[index].url);
                tracing::info!("Downloading chapter '{title}' ({} of {total})", offset + 1);
                let page = client.get_text(&url).await?;
                let cleaned = clean_chapter_html(&page)
                    .wrap_err_with(|| format!("failed to parse chapter '{title}'"))?;
                let chapter = &mut self.chapters[index];
                chapter.content = Some(cleaned.content);
                chapter.authors_note_start = cleaned.note_start;
                chapter.authors_note_end = cleaned.note_end;
            }

            if !already {
                // Persist each finished chapter so a later failure resumes
                // instead of starting over. The visible checkpoint message
                // stays on the existing every-100-chapter cadence.
                Cache::save_download_progress(self.id, offset + 1, &ids)?;
                Cache::write_book(self)?;
            }
            if (offset + 1) % 100 == 0 {
                tracing::info!("Saving checkpoint at chapter {} of {total}", offset + 1);
            }
            if !already && offset + 1 != total {
                let jitter = rand::thread_rng().gen_range(0..400u64);
                tokio::time::sleep(Duration::from_millis(600 + jitter)).await;
            }
        }
        Ok(())
    }

    pub fn into_epub_slice(mut self, selection: &Selection, query: &ChapterQuery) -> Self {
        self.chapters = self
            .chapters
            .drain(selection.start..=selection.end)
            .collect();
        if query.is_ranged() {
            self.epub_title = Some(format!("{} ({})", self.title, selection.label));
            self.range_label = Some(selection.label.clone());
            self.range_from = query.from.clone();
            self.range_to = query.to.clone();
            self.range_after = query.after.clone();
            if let Some(title) = &self.epub_title {
                tracing::info!("EPUB title: {title}");
            }
        }
        self
    }
}

struct CoverChoice {
    area: u128,
    bytes: Vec<u8>,
}

pub async fn write_epub(
    client: &HttpClient,
    book: &Book,
    outfile: Option<String>,
) -> eyre::Result<()> {
    if book
        .chapters
        .iter()
        .any(|chapter| chapter.content.is_none())
    {
        bail!("Refusing to write an EPUB with chapters that have no content");
    }
    if book
        .cover
        .as_ref()
        .map(|bytes| bytes.is_empty())
        .unwrap_or(true)
    {
        bail!("Book has no cover image");
    }

    let display = display_title(book);
    let outfile = match outfile {
        Some(outfile) => outfile,
        None => format!("{}.epub", sanitize_filename(&display)),
    };

    let temp_folder = tempfile::tempdir()?;
    let epub_path = temp_folder
        .path()
        .join(Uuid::new_v4().to_string())
        .with_extension("epub");
    tracing::debug!("Writing epub to {:?}", epub_path);
    let file = std::fs::File::create(&epub_path)?;
    let mut epub_file = zip::ZipWriter::new(file);
    let stored =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::FileOptions::default();

    epub_file.start_file("mimetype", stored)?;
    epub_file.write_all(b"application/epub+zip")?;
    epub_file.add_directory("META-INF", deflated)?;
    epub_file.start_file("META-INF/container.xml", deflated)?;
    container_xml(&mut epub_file)?;
    epub_file.start_file("OEBPS/content.opf", deflated)?;
    content_opf(book, &mut epub_file)?;
    epub_file.start_file("OEBPS/toc.ncx", deflated)?;
    toc_ncx(book, &mut epub_file)?;
    epub_file.start_file("OEBPS/nav.xhtml", deflated)?;
    nav_xhtml(book, &mut epub_file)?;

    let cover = cover_info(book.cover.as_deref().unwrap_or_default());
    for chapter in &book.chapters {
        epub_file.start_file(format!("OEBPS/text/{}.xhtml", chapter.id), deflated)?;
        chapter_html(chapter, &mut epub_file)?;

        let mut images = parse_images(chapter.content.as_deref().unwrap_or(""))?;
        images.extend(parse_images(
            chapter.authors_note_start.as_deref().unwrap_or(""),
        )?);
        images.extend(parse_images(
            chapter.authors_note_end.as_deref().unwrap_or(""),
        )?);
        if !images.is_empty() {
            tracing::info!("Downloading inline images for chapter '{}'.", chapter.title);
            for (index, url) in images.iter().enumerate() {
                if index > 0 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                epub_file.start_file(
                    format!("OEBPS/images/{}", extract_image_name(url)?),
                    deflated,
                )?;
                download_image(client, book, url.clone(), &mut epub_file).await?;
            }
        }
    }

    epub_file.start_file(format!("OEBPS/images/{}", cover.filename), deflated)?;
    epub_file.write_all(book.cover.as_deref().unwrap_or_default())?;

    epub_file.start_file("OEBPS/text/title.xhtml", deflated)?;
    title_html(book, &mut epub_file)?;
    epub_file.start_file("OEBPS/styles/stylesheet.css", deflated)?;
    epub_file.write_all(include_bytes!("./assets/styles.css"))?;

    epub_file.finish()?;
    tracing::debug!("Copying epub from {:?} to {:?}", epub_path, outfile);
    std::fs::copy(epub_path, &outfile)?;
    tracing::info!("Wrote EPUB to {outfile}");
    Ok(())
}

fn display_title(book: &Book) -> String {
    book.epub_title
        .clone()
        .unwrap_or_else(|| book.title.clone())
}

fn book_identifier(book: &Book) -> String {
    match &book.range_label {
        Some(label) => format!("{}-{}", book.id, label.replace(' ', "-")),
        None => book.id.to_string(),
    }
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-")
}

struct CoverFile {
    filename: &'static str,
    media_type: &'static str,
}

fn cover_info(bytes: &[u8]) -> CoverFile {
    if is_png(bytes) {
        CoverFile {
            filename: "cover.png",
            media_type: "image/png",
        }
    } else if is_gif(bytes) {
        CoverFile {
            filename: "cover.gif",
            media_type: "image/gif",
        }
    } else {
        CoverFile {
            filename: "cover.jpg",
            media_type: "image/jpeg",
        }
    }
}

fn title_html(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let title = display_title(book);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    let cover = cover_info(book.cover.as_deref().unwrap_or_default());
    let src = format!("../images/{}", cover.filename);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .attr("href", "../styles/stylesheet.css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("img")
                .attr("src", &src)
                .attr("alt", "Cover")
                .attr("class", "cover")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("h1").attr("class", "title").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("h2").attr("class", "author").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn chapter_html(chapter: &Chapter, file: &mut impl Write) -> eyre::Result<()> {
    let mut config = EmitterConfig::new().perform_indent(true);
    config.perform_escaping = false;
    let mut xml = config.create_writer(file);
    let title = xml_text(&chapter.title);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .ns("epub", "http://www.idpf.org/2007/ops")
                .attr("xml:lang", "en")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "generator")
                .attr("content", "text/html; charset=UTF-8")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("href", "../styles/stylesheet.css")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("h1")
                .attr("class", "chapter-title")
                .into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
        ],
    )?;
    if let Some(note) = note_html(&chapter.authors_note_start) {
        write_note(&mut xml, "authors-note-start", note)?;
    }
    if let Some(mut content) = chapter.content.clone() {
        let font_family_regex = Regex::new(r"font-family:.*?;").unwrap();
        content = font_family_regex.replace_all(&content, "").to_string();
        let font_weight_regex = Regex::new(r"font-weight:\s*normal").unwrap();
        content = font_weight_regex.replace_all(&content, "").to_string();
        let font_weight_regex = Regex::new(r"font-weight:\s*400").unwrap();
        content = font_weight_regex.replace_all(&content, "").to_string();
        let overflow_regex = Regex::new(r"overflow:\s*auto").unwrap();
        content = overflow_regex.replace_all(&content, "").to_string();
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("div")
                    .attr("class", "chapter-content")
                    .into(),
                XmlEvent::characters(&rewrite_images(content)?),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    if let Some(note) = note_html(&chapter.authors_note_end) {
        write_note(&mut xml, "authors-note-end", note)?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn note_html(note: &Option<String>) -> Option<&str> {
    note.as_deref().filter(|html| !html.trim().is_empty())
}

fn write_note(
    xml: &mut xml::EventWriter<&mut impl Write>,
    class: &str,
    html: &str,
) -> eyre::Result<()> {
    write_elements(
        xml,
        vec![
            XmlEvent::start_element("aside")
                .attr("class", class)
                .attr("epub:type", "note")
                .into(),
            XmlEvent::start_element("p")
                .attr("class", "authors-note-label")
                .into(),
            XmlEvent::characters("Author's Note"),
            XmlEvent::end_element().into(),
            XmlEvent::characters(html),
            XmlEvent::end_element().into(),
        ],
    )
}

fn container_xml(file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("container")
                .attr("version", "1.0")
                .ns("a", "urn:oasis:names:tc:opendocument:xmlns:container")
                .into(),
            XmlEvent::start_element("rootfiles").into(),
            XmlEvent::start_element("rootfile")
                .attr("full-path", "OEBPS/content.opf")
                .attr("media-type", "application/oebps-package+xml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn content_opf(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let title = display_title(book);
    let identifier = book_identifier(book);
    let cover = cover_info(book.cover.as_deref().unwrap_or_default());
    let cover_href = format!("images/{}", cover.filename);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("package")
                .ns("", "http://www.idpf.org/2007/opf")
                .attr("version", "3.0")
                .attr("unique-identifier", "bookid")
                .into(),
            XmlEvent::start_element("metadata")
                .ns("dc", "http://purl.org/dc/elements/1.1/")
                .into(),
            XmlEvent::start_element("dc:title").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:creator").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:description").into(),
            XmlEvent::characters(&book.description),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:date").into(),
            XmlEvent::characters(&book.date_published),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:identifier")
                .attr("id", "bookid")
                .into(),
            XmlEvent::characters(&identifier),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:language").into(),
            XmlEvent::characters("en"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "cover")
                .attr("content", "cover")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "primary-writing-mode")
                .attr("content", "horizontal-lr")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "rr-to-epub:royal-road-id")
                .attr("content", &book.id.to_string())
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    write_optional_meta(&mut xml, "rr-to-epub:source-url", &book.source_url)?;
    if let Some(value) = &book.range_from {
        write_optional_meta(&mut xml, "rr-to-epub:range-from", value)?;
    }
    if let Some(value) = &book.range_to {
        write_optional_meta(&mut xml, "rr-to-epub:range-to", value)?;
    }
    if let Some(value) = &book.range_after {
        write_optional_meta(&mut xml, "rr-to-epub:range-after", value)?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::start_element("manifest").into(),
            XmlEvent::start_element("item")
                .attr("id", "title")
                .attr("href", "text/title.xhtml")
                .attr("media-type", "application/xhtml+xml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("item")
                .attr("id", "cover")
                .attr("href", &cover_href)
                .attr("media-type", cover.media_type)
                .attr("properties", "cover-image")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("item")
                .attr("id", "stylesheet")
                .attr("href", "styles/stylesheet.css")
                .attr("media-type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("item")
                .attr("id", "ncx")
                .attr("href", "toc.ncx")
                .attr("media-type", "application/x-dtbncx+xml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("item")
                .attr("id", "nav")
                .attr("href", "nav.xhtml")
                .attr("media-type", "application/xhtml+xml")
                .attr("properties", "nav")
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    for chapter in &book.chapters {
        let id = chapter.id.to_string();
        let href = format!("text/{}.xhtml", chapter.id);
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("item")
                    .attr("id", &id)
                    .attr("href", &href)
                    .attr("media-type", "application/xhtml+xml")
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::start_element("spine").attr("toc", "ncx").into(),
            XmlEvent::start_element("itemref")
                .attr("idref", "title")
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    for chapter in &book.chapters {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("itemref")
                    .attr("idref", &chapter.id.to_string())
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::start_element("guide").into(),
            XmlEvent::start_element("reference")
                .attr("type", "cover")
                .attr("title", "Cover")
                .attr("href", "text/title.xhtml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn write_optional_meta(
    xml: &mut xml::EventWriter<&mut impl Write>,
    name: &str,
    content: &str,
) -> eyre::Result<()> {
    if content.is_empty() {
        return Ok(());
    }
    write_elements(
        xml,
        vec![
            XmlEvent::start_element("meta")
                .attr("name", name)
                .attr("content", content)
                .into(),
            XmlEvent::end_element().into(),
        ],
    )
}

fn toc_ncx(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let title = display_title(book);
    let identifier = book_identifier(book);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("ncx")
                .ns("", "http://www.daisy.org/z3986/2005/ncx/")
                .attr("version", "2005-1")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:uid")
                .attr("content", &identifier)
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:depth")
                .attr("content", "1")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:totalPageCount")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:maxPageNumber")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("docTitle").into(),
            XmlEvent::start_element("text").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("navMap").into(),
            XmlEvent::start_element("navPoint")
                .attr("id", "cover")
                .attr("playOrder", "0")
                .into(),
            XmlEvent::start_element("navLabel").into(),
            XmlEvent::start_element("text").into(),
            XmlEvent::characters("Cover"),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("content")
                .attr("src", "text/title.xhtml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    for (index, chapter) in book.chapters.iter().enumerate() {
        let id = chapter.id.to_string();
        let play_order = (index + 1).to_string();
        let src = format!("text/{}.xhtml", chapter.id);
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("navPoint")
                    .attr("id", &id)
                    .attr("playOrder", &play_order)
                    .into(),
                XmlEvent::start_element("navLabel").into(),
                XmlEvent::start_element("text").into(),
                XmlEvent::characters(&chapter.title),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
                XmlEvent::start_element("content").attr("src", &src).into(),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn nav_xhtml(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let title = display_title(book);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .ns("epub", "http://www.idpf.org/2007/ops")
                .attr("xml:lang", "en")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&title),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("nav")
                .attr("epub:type", "toc")
                .attr("id", "toc")
                .into(),
            XmlEvent::start_element("h1").into(),
            XmlEvent::characters("Table of Contents"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("ol").into(),
            XmlEvent::start_element("li").into(),
            XmlEvent::start_element("a")
                .attr("href", "text/title.xhtml")
                .into(),
            XmlEvent::characters("Cover"),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    for chapter in &book.chapters {
        let href = format!("text/{}.xhtml", chapter.id);
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("li").into(),
                XmlEvent::start_element("a").attr("href", &href).into(),
                XmlEvent::characters(&chapter.title),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn normalized_text(element: scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn fiction_page_url(id: u32, source: Option<&str>) -> String {
    if let Some(source) = source {
        if let Ok(url) = Url::parse(source) {
            let segments: Vec<&str> = url
                .path_segments()
                .map(|parts| parts.filter(|part| !part.is_empty()).collect())
                .unwrap_or_default();
            if segments.len() >= 3
                && segments[0] == "fiction"
                && segments[1].parse::<u32>().ok() == Some(id)
                && segments[2] != "chapter"
            {
                return format!("https://www.royalroad.com/fiction/{id}/{}", segments[2]);
            }
        }
    }
    format!("https://www.royalroad.com/fiction/{id}")
}

fn chapter_page_url(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else if path.starts_with('/') {
        format!("https://www.royalroad.com{path}")
    } else {
        format!("https://www.royalroad.com/{path}")
    }
}

fn extract_js_string(html: &str, marker: &str) -> Option<String> {
    let index = html.find(marker)?;
    let rest = html[index + marker.len()..].trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(decoded) = char::from_u32(code) {
                            out.push(decoded);
                        }
                    }
                }
                Some(other) => out.push(other),
                None => break,
            }
            continue;
        }
        if ch == '"' {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

fn extract_js_json<'a>(html: &'a str, marker: &str) -> eyre::Result<&'a str> {
    let index = html
        .find(marker)
        .ok_or_else(|| eyre::eyre!("missing {marker}"))?;
    let rest = html[index + marker.len()..].trim_start();
    if !rest.starts_with('[') && !rest.starts_with('{') {
        bail!("{marker} was not a JSON value");
    }
    let end = json_value_end(rest)?;
    Ok(&rest[..end])
}

fn json_value_end(value: &str) -> eyre::Result<usize> {
    let bytes = value.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            match byte {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
    }
    bail!("unterminated JSON value");
}

pub(crate) fn cover_candidates(primary: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut push = |url: String| {
        if !urls.iter().any(|existing: &String| existing == &url) {
            urls.push(url);
        }
    };
    push(primary.to_string());
    if let Some((prefix, rest)) = split_cover(primary) {
        for size in ["covers-large", "covers-full", "covers-medium"] {
            push(format!("{prefix}/{size}/{rest}"));
        }
    }
    urls
}

fn split_cover(url: &str) -> Option<(&str, &str)> {
    for size in [
        "covers-large",
        "covers-full",
        "covers-medium",
        "covers-small",
    ] {
        let token = format!("/{size}/");
        if let Some(index) = url.find(&token) {
            let rest = &url[index + token.len()..];
            if !rest.is_empty() {
                return Some((&url[..index], rest));
            }
        }
    }
    None
}

fn prepare_cover_bytes(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    if is_html(bytes) {
        bail!("cover response was HTML");
    }
    if is_jpeg(bytes) || is_png(bytes) || is_gif(bytes) {
        return Ok(bytes.to_vec());
    }
    if is_webp(bytes) {
        let Some(webp) = Decoder::new(bytes).decode() else {
            bail!("failed to decode webp cover");
        };
        let image = webp.to_image();
        let mut buffer = Vec::new();
        image.write_with_encoder(JpegEncoder::new_with_quality(Cursor::new(&mut buffer), 90))?;
        return Ok(buffer);
    }
    bail!("unsupported cover image");
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn stored_book_from_opf(contents: &str) -> Option<StoredBook> {
    let mut parsed = xml::EventReader::new(contents.as_bytes());
    let mut id = None;
    let mut source_url = None;
    let mut range_from = None;
    let mut range_to = None;
    let mut range_after = None;
    while let Ok(event) = parsed.next() {
        if matches!(event, xml::reader::XmlEvent::EndDocument) {
            break;
        }
        let xml::reader::XmlEvent::StartElement { attributes, .. } = event else {
            continue;
        };
        let name = attributes
            .iter()
            .find(|attr| attr.name.local_name == "name")
            .map(|attr| attr.value.as_str());
        let content = attributes
            .iter()
            .find(|attr| attr.name.local_name == "content")
            .map(|attr| attr.value.clone());
        let (Some(name), Some(content)) = (name, content) else {
            continue;
        };
        let content = non_empty(content);
        match name {
            "rr-to-epub:royal-road-id" => id = content.and_then(|value| value.parse().ok()),
            "rr-to-epub:source-url" => source_url = content,
            "rr-to-epub:range-from" => range_from = content,
            "rr-to-epub:range-to" => range_to = content,
            "rr-to-epub:range-after" => range_after = content,
            _ => {}
        }
    }
    id.map(|id| StoredBook {
        id,
        source_url,
        range_from,
        range_to,
        range_after,
    })
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_image_name(url: &str) -> eyre::Result<String> {
    let mut url = Url::parse(url)?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url
        .path_segments()
        .ok_or_else(|| eyre::eyre!("Invalid image URL"))?
        .next_back()
        .ok_or_else(|| eyre::eyre!("Invalid image URL"))?
        .to_string())
}

fn parse_images(body: &str) -> eyre::Result<Vec<String>> {
    let parsed = Html::parse_fragment(body);
    let selector = Selector::parse("img").unwrap();
    let mut sources = Vec::new();
    for element in parsed.select(&selector) {
        if let Some(src) = element.value().attr("src") {
            sources.push(src.to_string());
        }
    }
    Ok(sources)
}

fn rewrite_images(mut body: String) -> eyre::Result<String> {
    let parsed = Html::parse_fragment(&body);
    let selector = Selector::parse("img").unwrap();
    for element in parsed.select(&selector) {
        if let Some(src) = element.value().attr("src") {
            let new_src = format!("../images/{}", extract_image_name(src)?);
            body = body.replace(src, &new_src);
        }
    }
    Ok(body)
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
}
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() > 11
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
}
fn is_gif(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 && bytes[3] == 0x38
}
fn is_svg(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let trimmed = text.trim_start().to_ascii_lowercase();
        trimmed.starts_with("<?xml") || trimmed.starts_with("<svg")
    } else {
        false
    }
}
fn is_html(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let trimmed = text.trim_start().to_ascii_lowercase();
        trimmed.starts_with("<!doctype html>") || trimmed.starts_with("<html")
    } else {
        false
    }
}

async fn download_image(
    client: &HttpClient,
    book: &Book,
    url: String,
    file: &mut impl Write,
) -> eyre::Result<()> {
    let filename = match extract_image_name(&url) {
        Ok(filename) => filename,
        Err(err) => {
            tracing::warn!("Skipping inline image {url}: {err}");
            return Ok(());
        }
    };
    if let Some(image) = Cache::read_inline_image(book, &filename)? {
        file.write_all(&image)?;
        return Ok(());
    }
    let bytes = match client.get_bytes(&url).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                "Failed to download image from URL ({url}): {err}. This is likely NOT a bug with rr-to-epub."
            );
            return Ok(());
        }
    };
    if is_html(&bytes) {
        return Ok(());
    }
    if is_gif(&bytes) || is_svg(&bytes) {
        file.write_all(&bytes)?;
        return Ok(());
    }
    let image = if is_webp(&bytes) {
        let Some(webp) = Decoder::new(&bytes).decode() else {
            tracing::warn!(
                "Failed to decode webp image. Please report this as a bug and include the following URL: {url}"
            );
            return Ok(());
        };
        webp.to_image()
    } else if is_jpeg(&bytes) || is_png(&bytes) {
        ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()?
            .decode()?
    } else {
        bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {url}");
    };

    let width = image.width();
    let height = image.height();
    let image = image.resize(
        600,
        600 * height / width.max(1),
        image::imageops::FilterType::Lanczos3,
    );
    let mut buffer = Vec::new();
    if is_png(&bytes) || is_webp(&bytes) {
        image.write_with_encoder(PngEncoder::new_with_quality(
            Cursor::new(&mut buffer),
            CompressionType::Fast,
            FilterType::Adaptive,
        ))?;
    } else if is_jpeg(&bytes) {
        image.write_with_encoder(JpegEncoder::new_with_quality(Cursor::new(&mut buffer), 80))?;
    } else {
        bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {url}");
    }
    Cache::write_inline_image(book, &filename, &buffer)?;
    file.write_all(&buffer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpClient;

    fn sample_book() -> Book {
        Book {
            id: 33844,
            title: "The Runesmith".into(),
            author: "Kuropon".into(),
            description: "A story.".into(),
            date_published: "2020-07-19T16:26:01Z".into(),
            cover_url:
                "https://www.royalroadcdn.com/public/covers-large/33844-the-runesmith.jpg?time=1"
                    .into(),
            cover: Some(vec![0xFF, 0xD8, 0xFF, 0xD9]),
            source_url: "https://www.royalroad.com/fiction/33844/the-runesmith".into(),
            chapters: vec![
                Chapter {
                    id: 11,
                    title: " Chapter 266: Learning on the Job".into(),
                    content: Some(
                        "<p>Hello <em>there</em> <strong>friend</strong> <i>now</i>.</p>".into(),
                    ),
                    authors_note_start: Some("<p>Thanks for reading.</p>".into()),
                    ..Chapter::default()
                },
                Chapter {
                    id: 12,
                    title: "Chapter 267: Sworn In".into(),
                    content: Some("<p>Next.</p>".into()),
                    authors_note_end: Some("<p>See you.</p>".into()),
                    ..Chapter::default()
                },
            ],
            epub_title: Some("The Runesmith (Ch 266-267)".into()),
            range_label: Some("Ch 266-267".into()),
            range_from: Some("266".into()),
            range_to: Some("267".into()),
            ..Book::default()
        }
    }

    #[test]
    fn fiction_url_keeps_the_slug() {
        assert_eq!(
            fiction_page_url(
                33844,
                Some("https://www.royalroad.com/fiction/33844/the-runesmith")
            ),
            "https://www.royalroad.com/fiction/33844/the-runesmith"
        );
        assert_eq!(
            fiction_page_url(
                33844,
                Some("https://www.royalroad.com/fiction/33844/the-runesmith/chapter/1/slug")
            ),
            "https://www.royalroad.com/fiction/33844/the-runesmith"
        );
        assert_eq!(
            fiction_page_url(33844, None),
            "https://www.royalroad.com/fiction/33844"
        );
    }

    #[test]
    fn cover_candidates_try_larger_sizes_once() {
        let urls = cover_candidates(
            "https://www.royalroadcdn.com/public/covers-large/33844-the-runesmith.jpg?time=1",
        );
        assert_eq!(
            urls.iter()
                .filter(|url| url.contains("/covers-large/"))
                .count(),
            1
        );
        assert!(urls
            .iter()
            .any(|url| url.contains("/covers-full/33844-the-runesmith.jpg?time=1")));
        assert!(urls.iter().any(|url| url.contains("/covers-medium/")));
    }

    #[test]
    fn opf_lists_every_chapter_cover_and_range() {
        let book = sample_book();
        let mut opf = Vec::new();
        content_opf(&book, &mut opf).unwrap();
        let opf = String::from_utf8(opf).unwrap();
        assert!(opf.contains("The Runesmith (Ch 266-267)"));
        assert!(opf.contains("properties=\"cover-image\""));
        assert!(opf.contains("rr-to-epub:royal-road-id"));
        assert!(opf.contains("content=\"33844\""));
        assert!(opf.contains("rr-to-epub:range-from"));
        assert!(opf.contains("content=\"266\""));
        assert_eq!(opf.matches("idref=").count(), 3);
        let stored = stored_book_from_opf(&opf).unwrap();
        assert_eq!(stored.id, 33844);
        assert_eq!(stored.range_from.as_deref(), Some("266"));
        assert_eq!(stored.range_to.as_deref(), Some("267"));
        assert!(stored
            .source_url
            .unwrap()
            .contains("/fiction/33844/the-runesmith"));

        let mut ncx = Vec::new();
        toc_ncx(&book, &mut ncx).unwrap();
        let ncx = String::from_utf8(ncx).unwrap();
        assert_eq!(ncx.matches("<navPoint").count(), 3);
        assert!(ncx.contains(" Chapter 266: Learning on the Job"));
        assert!(ncx.contains("Chapter 267: Sworn In"));

        let mut nav = Vec::new();
        nav_xhtml(&book, &mut nav).unwrap();
        let nav = String::from_utf8(nav).unwrap();
        assert!(nav.contains(" Chapter 266: Learning on the Job"));
        assert!(nav.contains("text/12.xhtml"));
    }

    #[test]
    fn chapter_xhtml_marks_notes_and_keeps_inline_formatting() {
        let chapter = &sample_book().chapters[0];
        let mut xhtml = Vec::new();
        chapter_html(chapter, &mut xhtml).unwrap();
        let xhtml = String::from_utf8(xhtml).unwrap();
        assert!(xhtml.contains("Author's Note"));
        assert!(xhtml.contains("epub:type=\"note\""));
        assert!(xhtml.contains("authors-note-start"));
        assert!(xhtml.contains("<em>there</em>"));
        assert!(xhtml.contains("<strong>friend</strong>"));
        assert!(xhtml.contains("<i>now</i>"));
        assert!(xhtml.contains("<p>"));
        assert!(xhtml.contains(" Chapter 266: Learning on the Job"));
    }

    #[tokio::test]
    async fn epub_zip_embeds_cover_and_one_spine_item_per_chapter() {
        let book = sample_book();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.epub");
        let client = HttpClient::new().unwrap();
        write_epub(&client, &book, Some(path.to_string_lossy().into_owned()))
            .await
            .unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        {
            let mime = zip.by_name("mimetype").unwrap();
            assert_eq!(mime.compression(), zip::CompressionMethod::Stored);
        }
        let cover = {
            let mut entry = zip.by_name("OEBPS/images/cover.jpg").unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            bytes
        };
        assert!(is_jpeg(&cover));
        let chapter = {
            let mut entry = zip.by_name("OEBPS/text/11.xhtml").unwrap();
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            text
        };
        assert!(chapter.contains("<em>there</em>"));
        assert!(chapter.contains("Author's Note"));
        assert!(zip.by_name("OEBPS/text/12.xhtml").is_ok());
        assert!(zip.by_name("OEBPS/nav.xhtml").is_ok());
    }
}
