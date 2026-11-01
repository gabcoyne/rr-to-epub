# rr-to-epub

> [!NOTE]  
> All stories are the property of their respective authors. This application is not affiliated with Royal Road in any way and makes no attempt to claim credit for any stories downloaded. **If anyone working at Royal Road wants this tool taken down, please reach out to me at the email linked to my Github and I will immediately comply.**

A small application to convert a [Royal Road](https://www.royalroad.com/) story to an `.epub` file, compatible with readers such as Kindle or Calibre. Motivated by me wanting to read some stories on my Kindle when I'm without access to the Royal Road web service.

This fork can download a slice of a fiction (for example, from the chapter where you stopped) and keeps interludes that sit between those bounds in table-of-contents order.

## Installation

### From Source

This tool is written in Rust. Install [Rust](https://www.rust-lang.org/tools/install) (stable, 1.98 or newer), then run:

```sh
cargo install --locked --git https://github.com/gabcoyne/rr-to-epub
```

Prebuilt binaries are not currently available.

## Usage

Download a whole book:

```sh
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith
```

Download a table-of-contents slice. Indexes are 1-based positions in Royal Road's chapter list, so side chapters between the bounds are included:

```sh
# Chapters 672 through 723
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --from 672 --to 723

# From chapter 672 through whatever is latest
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --from 672

# Start after chapter 671 (that chapter itself is left out)
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --after 671
```

The same bounds accept a Royal Road chapter id or a chapter URL:

```sh
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --from id:3402749 --to id:4270013
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith \
  --from https://www.royalroad.com/fiction/33844/the-runesmith/chapter/3402749/chapter-672-first-apprentice-acquired
```

A partial book is titled like `The Runesmith (Ch 672-723)`. `update` reads that range back out of the EPUB and refreshes it. If you left the end open (`--from` without `--to`, or `--after` without `--to`), update keeps extending through the latest chapter.

```sh
rr-to-epub update <directory-or-epub>
```

`--ignore-cache` redownloads chapter text even when the cached copy is still current. Chapter requests are spaced out and retried. Royal Road is behind Cloudflare, which rejects a plain Rust HTTP client; this tool impersonates Chrome's TLS fingerprint instead of sending a custom User-Agent.

Full help: `rr-to-epub download --help`.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
