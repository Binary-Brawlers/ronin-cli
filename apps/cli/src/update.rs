use flate2::read::GzDecoder;
use futures::StreamExt;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::Path,
};
use tempfile::NamedTempFile;
use thiserror::Error;
use zip::ZipArchive;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_API: &str = "https://api.github.com/repos/Binary-Brawlers/ronin-cli/releases/latest";
const MAX_ARCHIVE_BYTES: usize = 25 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 50 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated { from: Version, to: Version },
    Current { version: Version },
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    Available { current: Version, latest: Version },
    Current { version: Version },
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("Ronin updates are not available for {0}")]
    UnsupportedPlatform(String),
    #[error("could not check for Ronin updates: {0}")]
    Request(#[from] reqwest::Error),
    #[error("the latest Ronin release is invalid: {0}")]
    InvalidRelease(String),
    #[error("the downloaded Ronin archive failed checksum verification")]
    ChecksumMismatch,
    #[error("the downloaded Ronin archive is invalid: {0}")]
    InvalidArchive(String),
    #[error("could not replace the Ronin executable: {0}")]
    Install(#[from] std::io::Error),
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub async fn update() -> Result<UpdateOutcome, UpdateError> {
    let target = platform_target(std::env::consts::OS, std::env::consts::ARCH)?;
    let archive_name = archive_name(std::env::consts::OS, target);
    let current = current_version()?;
    let client = release_client()?;
    let release = latest_release(&client).await?;
    let latest = release_version(&release.tag_name)?;
    if latest <= current {
        return Ok(UpdateOutcome::Current { version: current });
    }

    let archive_url = asset_url(&release, &archive_name)?;
    let checksum_url = asset_url(&release, "SHA256SUMS")?;
    let archive = download(&client, archive_url, MAX_ARCHIVE_BYTES).await?;
    let manifest = download(&client, checksum_url, MAX_CHECKSUM_BYTES).await?;
    let expected = checksum_for(&manifest, &archive_name)?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch);
    }

    let binary = extract_binary(&archive, std::env::consts::OS)?;
    replace_executable(&std::env::current_exe()?, &binary)?;
    Ok(UpdateOutcome::Updated {
        from: current,
        to: latest,
    })
}

/// Checks GitHub for a newer compatible release without downloading it.
pub async fn check() -> Result<UpdateCheck, UpdateError> {
    platform_target(std::env::consts::OS, std::env::consts::ARCH)?;
    let current = current_version()?;
    let client = release_client()?;
    let release = latest_release(&client).await?;
    let latest = release_version(&release.tag_name)?;
    Ok(classify_update(current, latest))
}

fn classify_update(current: Version, latest: Version) -> UpdateCheck {
    if latest > current {
        UpdateCheck::Available { current, latest }
    } else {
        UpdateCheck::Current { version: current }
    }
}

fn current_version() -> Result<Version, UpdateError> {
    Version::parse(VERSION).map_err(|error| UpdateError::InvalidRelease(error.to_string()))
}

fn release_client() -> Result<Client, UpdateError> {
    Ok(Client::builder()
        .user_agent(format!("ronin/{VERSION}"))
        .build()?)
}

async fn latest_release(client: &Client) -> Result<Release, UpdateError> {
    Ok(client
        .get(RELEASE_API)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn platform_target(os: &str, arch: &str) -> Result<&'static str, UpdateError> {
    match (os, arch) {
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(UpdateError::UnsupportedPlatform(format!("{os}/{arch}"))),
    }
}

fn archive_name(os: &str, target: &str) -> String {
    let extension = if os == "windows" { "zip" } else { "tar.gz" };
    format!("ronin-{target}.{extension}")
}

fn release_version(tag: &str) -> Result<Version, UpdateError> {
    let value = tag
        .strip_prefix("ronin-v")
        .ok_or_else(|| UpdateError::InvalidRelease(format!("unexpected release tag {tag}")))?;
    Version::parse(value).map_err(|error| UpdateError::InvalidRelease(error.to_string()))
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str, UpdateError> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .ok_or_else(|| UpdateError::InvalidRelease(format!("missing {name}")))
}

async fn download(client: &Client, url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(UpdateError::InvalidRelease("download is too large".into()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > limit {
            return Err(UpdateError::InvalidRelease("download is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn checksum_for(manifest: &[u8], archive_name: &str) -> Result<String, UpdateError> {
    let manifest = std::str::from_utf8(manifest)
        .map_err(|_| UpdateError::InvalidRelease("checksum manifest is not UTF-8".into()))?;
    let mut matches = manifest.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == archive_name && fields.next().is_none()).then_some(checksum)
    });
    let checksum = matches.next().ok_or_else(|| {
        UpdateError::InvalidRelease(format!("missing checksum for {archive_name}"))
    })?;
    if matches.next().is_some()
        || checksum.len() != 64
        || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(UpdateError::InvalidRelease(
            "checksum manifest contains an invalid entry".into(),
        ));
    }
    Ok(checksum.to_ascii_lowercase())
}

fn extract_binary(archive: &[u8], os: &str) -> Result<Vec<u8>, UpdateError> {
    if os == "windows" {
        return extract_zip_binary(archive);
    }
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let mut binary = None;
    let entries = archive
        .entries()
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        if path.as_ref() != Path::new("ronin") || !entry.header().entry_type().is_file() {
            return Err(UpdateError::InvalidArchive(
                "expected exactly one regular file named ronin".into(),
            ));
        }
        if binary.is_some() || entry.size() > MAX_BINARY_BYTES {
            return Err(UpdateError::InvalidArchive(
                "archive contains duplicate or oversized files".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        binary = Some(bytes);
    }
    binary.ok_or_else(|| UpdateError::InvalidArchive("archive is empty".into()))
}

fn extract_zip_binary(archive: &[u8]) -> Result<Vec<u8>, UpdateError> {
    let mut archive = ZipArchive::new(Cursor::new(archive))
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    if archive.len() != 1 {
        return Err(UpdateError::InvalidArchive(
            "expected exactly one regular file named ronin.exe".into(),
        ));
    }
    let mut entry = archive
        .by_index(0)
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    if entry.name() != "ronin.exe" || !entry.is_file() || entry.size() > MAX_BINARY_BYTES {
        return Err(UpdateError::InvalidArchive(
            "expected exactly one regular file named ronin.exe".into(),
        ));
    }
    let mut binary = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut binary)
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    Ok(binary)
}

fn replace_executable(destination: &Path, binary: &[u8]) -> Result<(), std::io::Error> {
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no parent directory"))?;
    let mut replacement = NamedTempFile::new_in(parent)?;
    replacement.write_all(binary)?;
    replacement.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        replacement
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    self_replace::self_replace(replacement.path())?;
    #[cfg(not(windows))]
    replacement
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn archive(path: &str, contents: &[u8], entry_type: tar::EntryType) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let encoder = GzEncoder::new(&mut output, Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(contents))
                .unwrap();
            builder.finish().unwrap();
        }
        output
    }

    fn zip_archive(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut archive = ZipWriter::new(&mut output);
            archive
                .start_file(path, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(contents).unwrap();
            archive.finish().unwrap();
        }
        output.into_inner()
    }

    #[test]
    fn maps_release_targets() {
        assert_eq!(
            platform_target("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            platform_target("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            platform_target("windows", "x86_64").unwrap(),
            "x86_64-pc-windows-msvc"
        );
        assert!(platform_target("windows", "aarch64").is_err());
        assert_eq!(
            archive_name("windows", "x86_64-pc-windows-msvc"),
            "ronin-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn parses_only_ronin_release_tags() {
        assert_eq!(
            release_version("ronin-v1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        assert!(release_version("v1.2.3").is_err());
        assert!(release_version("ronin-vlatest").is_err());
    }

    #[test]
    fn classifies_only_newer_versions_as_updates() {
        assert_eq!(
            classify_update(Version::new(1, 2, 3), Version::new(1, 3, 0)),
            UpdateCheck::Available {
                current: Version::new(1, 2, 3),
                latest: Version::new(1, 3, 0),
            }
        );
        assert_eq!(
            classify_update(Version::new(1, 2, 3), Version::new(1, 2, 3)),
            UpdateCheck::Current {
                version: Version::new(1, 2, 3),
            }
        );
        assert_eq!(
            classify_update(Version::new(2, 0, 0), Version::new(1, 9, 9)),
            UpdateCheck::Current {
                version: Version::new(2, 0, 0),
            }
        );
    }

    #[test]
    fn selects_an_exact_unique_checksum() {
        let name = "ronin-aarch64-apple-darwin.tar.gz";
        let hash = "a".repeat(64);
        assert_eq!(
            checksum_for(format!("{hash}  {name}\n").as_bytes(), name).unwrap(),
            hash
        );
        assert!(checksum_for(format!("{hash}  other-{name}\n").as_bytes(), name).is_err());
        assert!(
            checksum_for(format!("{hash}  {name}\n{hash}  {name}\n").as_bytes(), name).is_err()
        );
    }

    #[test]
    fn extracts_only_the_expected_regular_file() {
        assert_eq!(
            extract_binary(
                &archive("ronin", b"binary", tar::EntryType::Regular),
                "linux"
            )
            .unwrap(),
            b"binary"
        );
        assert!(extract_binary(
            &archive("other", b"binary", tar::EntryType::Regular),
            "linux"
        )
        .is_err());
        assert!(extract_binary(
            &archive("ronin", b"target", tar::EntryType::Symlink),
            "linux"
        )
        .is_err());
        assert_eq!(
            extract_binary(&zip_archive("ronin.exe", b"binary"), "windows").unwrap(),
            b"binary"
        );
        assert!(extract_binary(&zip_archive("other.exe", b"binary"), "windows").is_err());
    }

    #[test]
    fn replaces_a_binary_and_sets_executable_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("ronin");
        fs::write(&destination, b"old").unwrap();
        replace_executable(&destination, b"new").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(destination).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
}
