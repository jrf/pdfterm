use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

const PDFIUM_REVISION: &str = "7881";

struct Asset {
    name: &'static str,
    checksum: &'static str,
    library_name: &'static str,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Err(error) = download_pdfium() {
        panic!("could not prepare embedded PDFium: {error}");
    }
}

fn download_pdfium() -> Result<(), Box<dyn Error>> {
    let target_os = env::var("CARGO_CFG_TARGET_OS")?;
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    let asset = asset_for_target(&target_os, &target_arch)?;
    let output =
        PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join(asset.library_name);

    println!("cargo:rustc-env=PDFIUM_LIBRARY_NAME={}", asset.library_name);
    println!("cargo:rustc-env=PDFIUM_REVISION={PDFIUM_REVISION}");

    if output.is_file() {
        return Ok(());
    }

    let url = format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F{PDFIUM_REVISION}/{}.tgz",
        asset.name
    );
    println!("cargo:warning=downloading PDFium {PDFIUM_REVISION} for {target_os}/{target_arch}");
    let agent = Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut response = agent.get(&url).call()?;
    let bytes = response.body_mut().read_to_vec()?;
    let checksum = hex(&Sha256::digest(&bytes));
    if checksum != asset.checksum {
        return Err(io::Error::other(format!(
            "checksum mismatch for {}: expected {}, got {checksum}",
            asset.name, asset.checksum
        ))
        .into());
    }

    extract_library(&bytes, asset.library_name, &output)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn asset_for_target(target_os: &str, target_arch: &str) -> Result<Asset, Box<dyn Error>> {
    let asset = match (target_os, target_arch) {
        ("macos", "aarch64") => Asset {
            name: "pdfium-mac-arm64",
            checksum: "52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40",
            library_name: "libpdfium.dylib",
        },
        ("linux", "x86_64") => Asset {
            name: "pdfium-linux-x64",
            checksum: "1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d",
            library_name: "libpdfium.so",
        },
        ("linux", "aarch64") => Asset {
            name: "pdfium-linux-arm64",
            checksum: "ee7f7b7d5468958336a818c1cd580bdd20972846b7377b13f9a923d92d1d4674",
            library_name: "libpdfium.so",
        },
        _ => {
            return Err(io::Error::other(format!(
                "unsupported target: {target_os}/{target_arch}; supported targets are macos/aarch64, linux/x86_64, and linux/aarch64"
            ))
            .into());
        }
    };
    Ok(asset)
}

fn extract_library(
    archive_bytes: &[u8],
    library_name: &str,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    let decoder = GzDecoder::new(archive_bytes);
    let mut archive = Archive::new(decoder);
    let archive_path = Path::new("lib").join(library_name);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.ends_with(&archive_path) {
            let mut file = File::create(output)?;
            io::copy(&mut entry, &mut file)?;
            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} does not contain {}",
            archive_path.display(),
            library_name
        ),
    )
    .into())
}
