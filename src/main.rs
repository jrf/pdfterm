use std::io;
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

    /// Print the shared configuration as JSON (also creates the default file).
    #[arg(long)]
    print_config: bool,

    /// Print the licenses supplied with the embedded PDFium binary.
    #[arg(long)]
    third_party_licenses: bool,

    /// Resolve a source position and send it to an already-running viewer.
    #[arg(long, conflicts_with = "synctex_view", requires = "session")]
    forward_search: Option<PathBuf>,

    /// Resolve a source position and print the editor-neutral forward JSON request.
    #[arg(long)]
    synctex_view: Option<PathBuf>,

    /// One-based source line for --forward-search or --synctex-view.
    #[arg(long, default_value_t = 1)]
    line: u32,

    /// One-based Unicode character column in the source.
    #[arg(long, default_value_t = 1)]
    column: u32,

    /// Enable viewer control with named viewer/editor endpoints.
    #[arg(long)]
    session: Option<String>,
    /// Opaque launch identity used by the bundled Neovim adapter.
    #[arg(long, hide = true)]
    focus_token: Option<String>,

    /// Read newline-delimited PDF-point clicks and emit inverse SyncTeX JSON.
    #[arg(long, conflicts_with_all = ["forward_search", "synctex_view", "screenshot"])]
    synctex_edit_batch: bool,

    /// Save the running viewer's visible PDF viewport as a PNG.
    #[arg(long, requires = "session", conflicts_with_all = ["path", "forward_search", "synctex_view", "synctex_edit_batch"])]
    screenshot: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.third_party_licenses {
        return match std::io::Write::write_all(
            &mut std::io::stdout().lock(),
            include_bytes!(concat!(env!("OUT_DIR"), "/pdfium-notices.txt")),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if cli.synctex_edit_batch {
        let result = (|| -> Result<(), String> {
            let pdf = cli
                .path
                .as_deref()
                .ok_or("a PDF path is required for --synctex-edit-batch")?;
            let config = pdfterm::config::Config::load().map_err(|error| error.to_string())?;
            pdfterm::pdf::synctex_edit_batch(
                pdf,
                cli.pdfium_library.as_deref(),
                &config.viewer,
                io::stdin().lock(),
                io::stdout().lock(),
            )
        })();
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let mut config = match pdfterm::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = config.select_session(cli.session.as_deref()) {
        eprintln!("pdfterm: {error}");
        return ExitCode::FAILURE;
    }
    if cli.print_config {
        match serde_json::to_string(&config) {
            Ok(json) => {
                println!("{json}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("pdfterm: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some(path) = cli.screenshot.as_deref() {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let socket = config
                .forward_socket()
                .ok_or("forward_socket is disabled")?;
            pdfterm::ipc::screenshot(socket, path)?;
            Ok(())
        })();
        return match result {
            Ok(()) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(source) = cli.forward_search.as_ref().or(cli.synctex_view.as_ref()) {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let pdf = cli
                .path
                .as_ref()
                .ok_or("a PDF path is required for forward search")?;
            let request = pdfterm::synctex::resolve_forward_with_library(
                pdf,
                source,
                cli.line,
                cli.column,
                cli.pdfium_library.as_deref(),
            )?;
            if cli.synctex_view.is_some() {
                println!("{}", serde_json::to_string(&request)?);
            } else {
                pdfterm::ipc::forward(
                    config
                        .forward_socket()
                        .ok_or("forward_socket is disabled")?,
                    &request,
                )?;
            }
            Ok(())
        })();
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match pdfterm::app::run(
        cli.path,
        cli.pdfium_library,
        cli.page - 1,
        &config,
        cli.focus_token,
    ) {
        Ok(()) | Err(pdfterm::app::AppError::Quit) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            ExitCode::FAILURE
        }
    }
}
