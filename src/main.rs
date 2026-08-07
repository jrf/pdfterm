use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// PDF file to open. Omit to open the file picker.
    path: Option<PathBuf>,

    /// Path to libpdfium.dylib, libpdfium.so, or pdfium.dll.
    #[arg(long)]
    pdfium_library: Option<PathBuf>,

    /// One-based page number to show first.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    page: u32,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = pdfterm::config::Config::load();
    match pdfterm::app::run(cli.path, cli.pdfium_library, cli.page - 1, &config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            ExitCode::FAILURE
        }
    }
}
