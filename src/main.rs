use crate::api::RoyalRoadApi;
use crate::epub::write_epub;
use crate::range::RangeRequest;
use clap::Parser;
use epub::Book;
use url::Url;

mod api;
mod cache;
mod clean;
mod epub;
mod http;
mod range;
mod xml_ext;

#[derive(Debug, Parser)]
pub struct DownloadArgs {
    /// The file path to write the EPUB to. Defaults to <book-title>.epub.
    /// Must end in .epub for parsing to work correctly in most applications.
    pub output_file: Option<String>,

    #[arg(long = "book-id", short = 'b')]
    /// The ID of the book to download.  One of --book-id or --book-url is required.
    pub book_id: Option<u32>,

    #[arg(long = "book-url", short = 'u')]
    /// The URL of the book to download. One of --book-id or --book-url is required.
    pub book_url: Option<String>,

    /// First chapter to include. A number is a 1-based TOC index (`672`).
    /// `id:3402749` is a Royal Road chapter id. A chapter URL works too.
    #[arg(long, value_name = "BOUND", conflicts_with = "after")]
    pub from: Option<String>,

    /// Last chapter to include, in the same forms as --from. Defaults to the latest chapter.
    #[arg(long, value_name = "BOUND")]
    pub to: Option<String>,

    /// Start after this chapter (exclusive). Interludes between it and --to stay in TOC order.
    #[arg(long, value_name = "BOUND", conflicts_with = "from")]
    pub after: Option<String>,
}

#[derive(Debug, Parser)]
pub struct UpdateArgs {
    /// The folder of books to update, or the path to a single book file.
    pub folder_or_file: String,
}

#[derive(Debug, Parser)]
pub struct GlobalArgs {
    #[arg(long = "ignore-cache", global = true)]
    /// Ignore the cache and redownload all chapters even if the book wasn't modified.
    pub ignore_cache: bool,
}

#[derive(Debug, Parser)]
pub enum Command {
    #[clap(name = "download", about = "Download a book from Royal Road.")]
    Download(DownloadArgs),
    #[clap(name = "update", about = "Update all books in a folder.")]
    Update(UpdateArgs),
}

#[derive(Debug, Parser)]
#[clap(about, version, author)]
pub struct App {
    #[clap(subcommand)]
    command: Command,

    #[clap(flatten)]
    global: GlobalArgs,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    if let Err(err) = run().await {
        tracing::error!("{:?}", err);
    }
}

async fn run() -> eyre::Result<()> {
    let app = App::parse();
    let command = app.command;
    match command {
        Command::Download(args) => download(app.global, args).await?,
        Command::Update(args) => update(app.global, args).await?,
    }

    Ok(())
}

async fn download(global_args: GlobalArgs, args: DownloadArgs) -> eyre::Result<()> {
    let id = match (args.book_id, args.book_url) {
        (Some(id), _) => id,
        (None, Some(url)) => {
            let Ok(url) = Url::parse(&url) else {
                tracing::error!("Invalid book URL: {}", url);
                std::process::exit(1);
            };
            let Ok(id) = url
                .path_segments()
                .and_then(|mut s| s.nth(1))
                .unwrap()
                .parse()
            else {
                tracing::error!("Invalid book URL: {}", url);
                std::process::exit(1);
            };
            id
        }
        _ => {
            tracing::error!("One of --book-id or --book-url is required.");
            std::process::exit(1);
        }
    };

    let api = RoyalRoadApi::new()?;
    let range = RangeRequest {
        from: args.from,
        to: args.to,
        after: args.after,
    };
    let book = api.get_book(id, global_args.ignore_cache, &range).await?;
    write_epub(&book, args.output_file).await?;
    Ok(())
}
async fn update(global_args: GlobalArgs, args: UpdateArgs) -> eyre::Result<()> {
    let api = RoyalRoadApi::new()?;
    let Ok(stat) = std::fs::metadata(&args.folder_or_file) else {
        tracing::error!(
            "Folder or file \"{}\" does not exist, aborting.",
            args.folder_or_file
        );
        return Ok(());
    };
    if stat.is_file() {
        let file_name = args.folder_or_file;
        if !file_name.ends_with(".epub") {
            tracing::error!(
                "File \"{}\" is not an EPUB file or does not have the .epub extension, aborting.",
                file_name
            );
            return Ok(());
        }

        let path = std::path::Path::new(&file_name);
        let path = path.canonicalize()?;
        update_book(&api, &path, global_args.ignore_cache, Some(file_name)).await?;
    } else {
        let list = std::fs::read_dir(&args.folder_or_file)?;
        for file in list {
            let file = file?;
            let file_name = file
                .file_name()
                .into_string()
                .map_err(|_| eyre::eyre!("Invalid file name: {:?}", file.file_name()))?;
            if !file_name.ends_with(".epub") {
                continue;
            }
            update_book(
                &api,
                &file.path(),
                global_args.ignore_cache,
                Some(file.path().to_string_lossy().into_owned()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn update_book(
    api: &RoyalRoadApi,
    path: &std::path::Path,
    ignore_cache: bool,
    output: Option<String>,
) -> eyre::Result<()> {
    let origin = Book::origin_from_file(path)?;
    let Some(id) = origin.id else {
        tracing::warn!(
            "Found unmanaged book file \"{}\", skipping.",
            path.display()
        );
        return Ok(());
    };
    tracing::info!("Found book file \"{}\", updating.", path.display());
    let book = api.get_book(id, ignore_cache, &origin.range).await?;
    write_epub(&book, output).await?;
    Ok(())
}
