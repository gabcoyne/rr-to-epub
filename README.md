# rr-to-epub

> [!NOTE]  
> All stories are the property of their respective authors. This application is not affiliated with Royal Road in any way and makes no attempt to claim credit for any stories downloaded. **If anyone working at Royal Road wants this tool taken down, please reach out to me at the email linked to my Github and I will immediately comply.**

A small application to convert a [Royal Road](https://www.royalroad.com/) story to an `.epub` file, compatible with readers such as Kindle or Calibre. Motivated by me wanting to read some stories on my Kindle when I'm without access to the Royal Road web service.

## Installation

### From Source

This tool is written in Rust. To install it from source, first install [Rust](https://www.rust-lang.org/tools/install), then run the following command:

```sh
cargo install --locked --git https://github.com/isaac-mcfadyen/rr-to-epub
```

Prebuilt binaries are not currently available. 

## Usage

After installation, download a book by running the following command, substituting in the book URL:

```sh
rr-to-epub download -u <URL>
```

To update an entire folder of books (that were downloaded by rr-to-epub), run the following command:

```sh
rr-to-epub update -d <directory>
```

Full help can be found by running `rr-to-epub --help`.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
