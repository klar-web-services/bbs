use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{Read, Write},
    path::Path,
};
use tempfile::NamedTempFile;

const DEFAULT_REPOSITORY: &str = "klar-web-services/bbs";
pub const API_BASE: &str = "https://api.github.com";
pub const DOWNLOAD_BASE: &str = "https://github.com";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const ASSET: &str = "bbs-x86_64-unknown-linux-gnu.tar.gz";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const ASSET: &str = "bbs-aarch64-unknown-linux-gnu.tar.gz";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const ASSET: &str = "bbs-x86_64-apple-darwin.tar.gz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const ASSET: &str = "bbs-aarch64-apple-darwin.tar.gz";
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const ASSET: &str = "bbs-x86_64-pc-windows-msvc.zip";

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
compile_error!("bbs update has no published release asset for this target");

#[cfg(windows)]
pub const BINARY: &str = "bbs.exe";
#[cfg(not(windows))]
pub const BINARY: &str = "bbs";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let parts = trimmed.split('.').collect::<Vec<_>>();
        let invalid = || anyhow::anyhow!("release version `{text}` is not MAJOR.MINOR.PATCH");
        if parts.len() != 3 {
            return Err(invalid());
        }
        let mut numbers = [0u64; 3];
        for (slot, part) in numbers.iter_mut().zip(parts) {
            *slot = part.parse().map_err(|_| invalid())?;
        }
        Ok(Self {
            major: numbers[0],
            minor: numbers[1],
            patch: numbers[2],
        })
    }

    pub fn current() -> Result<Self> {
        Self::parse(env!("CARGO_PKG_VERSION"))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub fn repository() -> String {
    std::env::var("BBS_REPOSITORY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_REPOSITORY.to_owned())
}

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("bbs/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot build the update HTTP client")
}

/// A client for the *check*, with a short deadline.
///
/// This is deliberately separate from [`client`]: `download` fetches a
/// multi-megabyte archive through that one, and must not inherit a 3s
/// timeout.
pub fn checking_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("bbs/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .context("cannot build the update check HTTP client")
}

pub fn expected_digest(checksums: &str, asset: &str) -> Result<String> {
    for line in checksums.lines() {
        let mut fields = line.split_whitespace();
        let (Some(digest), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name.strip_prefix('*').unwrap_or(name) == asset {
            return Ok(digest.to_ascii_lowercase());
        }
    }
    bail!("release checksum is missing for {asset}")
}

pub fn verify(archive: &[u8], expected: &str) -> Result<()> {
    let actual = hex::encode(Sha256::digest(archive));
    if actual != expected.to_ascii_lowercase() {
        bail!("checksum verification failed for the downloaded {ASSET}");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("release archive is not readable")? {
        let mut entry = entry.context("release archive is not readable")?;
        let matches = entry
            .path()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .and_then(|name| name.to_str().map(str::to_owned))
            })
            .is_some_and(|name| name == BINARY);
        if matches {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    bail!("release archive does not contain a {BINARY} binary")
}

#[cfg(windows)]
pub fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .context("release archive is not readable")?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .context("release archive is not readable")?;
        let matches = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name == BINARY);
        if matches {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    bail!("release archive does not contain a {BINARY} binary")
}

pub fn replace(target: &Path, binary: &[u8]) -> Result<()> {
    let directory = target
        .parent()
        .context("the running binary has no parent directory")?;
    let mut temp = NamedTempFile::new_in(directory).with_context(|| {
        format!(
            "cannot write to {}; reinstall bbs with the install script instead",
            directory.display()
        )
    })?;
    temp.write_all(binary)?;
    temp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    persist(temp, target)
}

#[cfg(not(windows))]
fn persist(temp: NamedTempFile, target: &Path) -> Result<()> {
    temp.persist(target)
        .map_err(|error| anyhow::Error::new(error.error))
        .with_context(|| format!("cannot replace {}", target.display()))?;
    Ok(())
}

#[cfg(windows)]
fn persist(temp: NamedTempFile, target: &Path) -> Result<()> {
    let directory = target
        .parent()
        .context("the running binary has no parent directory")?;
    let backup = directory.join(format!("{BINARY}.old"));
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(target, &backup)
        .with_context(|| format!("cannot replace {}", target.display()))?;
    if let Err(error) = temp.persist(target) {
        let _ = std::fs::rename(&backup, target);
        return Err(anyhow::Error::new(error.error))
            .with_context(|| format!("cannot replace {}", target.display()));
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

pub async fn latest_version(
    http: &reqwest::Client,
    api_base: &str,
    repository: &str,
) -> Result<Version> {
    let url = format!(
        "{}/repos/{repository}/releases/latest",
        api_base.trim_end_matches('/')
    );
    let response = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        bail!(
            "GitHub refused the release lookup ({status}); unauthenticated requests are limited to 60 per hour"
        );
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("{repository} has no published releases");
    }
    if !status.is_success() {
        bail!("GitHub returned {status} for {url}");
    }
    let release: LatestRelease = response
        .json()
        .await
        .context("GitHub returned malformed release JSON")?;
    Version::parse(&release.tag_name)
}

pub async fn download(
    http: &reqwest::Client,
    download_base: &str,
    repository: &str,
    version: Version,
) -> Result<Vec<u8>> {
    let base = format!(
        "{}/{repository}/releases/download/v{version}",
        download_base.trim_end_matches('/')
    );
    let checksums = get(http, &format!("{base}/checksums.txt")).await?;
    let checksums = String::from_utf8(checksums).context("checksums.txt is not valid UTF-8")?;
    let archive = get(http, &format!("{base}/{ASSET}")).await?;
    verify(&archive, &expected_digest(&checksums, ASSET)?)?;
    Ok(archive)
}

async fn get(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !response.status().is_success() {
        bail!("GitHub returned {} for {url}", response.status());
    }
    Ok(response
        .bytes()
        .await
        .with_context(|| format!("cannot read {url}"))?
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_and_orders_release_versions() {
        assert_eq!(
            Version::parse("v0.2.0").unwrap(),
            Version {
                major: 0,
                minor: 2,
                patch: 0
            }
        );
        assert_eq!(Version::parse("0.2.0").unwrap().to_string(), "0.2.0");
        assert!(Version::parse("0.2.0").unwrap() > Version::parse("0.1.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
        assert_eq!(
            Version::parse("0.1.0").unwrap(),
            Version::parse("v0.1.0").unwrap()
        );
        for bad in ["0.1", "0.1.0-rc1", "abc", "", "0.1.0.1"] {
            assert!(Version::parse(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn the_current_version_is_parseable() {
        Version::current().expect("the crate version must be MAJOR.MINOR.PATCH");
    }

    #[test]
    fn asset_matches_the_published_release_names() {
        assert!(ASSET.starts_with("bbs-"));
        if cfg!(windows) {
            assert!(ASSET.ends_with(".zip"));
        } else {
            assert!(ASSET.ends_with(".tar.gz"));
        }
    }

    #[test]
    fn checksum_lookup_requires_an_exact_filename() {
        let checksums = concat!(
            "1111111111111111111111111111111111111111111111111111111111111111  bbs-x86_64-unknown-linux-gnu.tar.gz\n",
            "2222222222222222222222222222222222222222222222222222222222222222  bbs-x86_64-unknown-linux-gnu.tar.gz.sig\n",
            "3333333333333333333333333333333333333333333333333333333333333333  bbs-aarch64-unknown-linux-gnu.tar.gz\n",
        );
        assert_eq!(
            expected_digest(checksums, "bbs-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            expected_digest(checksums, "bbs-aarch64-unknown-linux-gnu.tar.gz").unwrap(),
            "3333333333333333333333333333333333333333333333333333333333333333"
        );
        assert!(expected_digest(checksums, "bbs-x86_64-pc-windows-msvc.zip").is_err());
    }

    #[test]
    fn checksum_lookup_tolerates_the_binary_mode_prefix() {
        let checksums =
            "4444444444444444444444444444444444444444444444444444444444444444 *bbs-test.tar.gz\n";
        assert_eq!(
            expected_digest(checksums, "bbs-test.tar.gz").unwrap(),
            "4444444444444444444444444444444444444444444444444444444444444444"
        );
    }

    #[test]
    fn verify_accepts_only_the_matching_digest() {
        let payload = b"release bytes";
        let digest = hex::encode(Sha256::digest(payload));
        assert!(verify(payload, &digest).is_ok());
        assert!(verify(payload, &digest.to_uppercase()).is_ok());
        assert!(verify(payload, &"0".repeat(64)).is_err());
    }

    #[cfg(not(windows))]
    fn archive_containing(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, contents).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(windows)]
    fn archive_containing(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn extracts_the_binary_from_a_release_archive() {
        let archive = archive_containing(BINARY, b"new binary");
        assert_eq!(extract(&archive).unwrap(), b"new binary");
    }

    #[test]
    fn extraction_rejects_an_archive_without_the_binary() {
        let archive = archive_containing("README.md", b"not a binary");
        assert!(extract(&archive).is_err());
    }

    #[test]
    fn replace_swaps_the_target_in_place() {
        let dir = tempdir().unwrap();
        let target = dir.path().join(BINARY);
        std::fs::write(&target, b"old binary").unwrap();

        replace(&target, b"new binary").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the replacement must stay executable");
        }
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary or backup files may remain");
    }

    // Windows ignores the read-only attribute on directories, so only unix can
    // create this condition without going through ACLs.
    #[cfg(unix)]
    #[test]
    fn replace_reports_the_directory_it_cannot_write() {
        let dir = tempdir().unwrap();
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        let target = readonly.join(BINARY);
        std::fs::write(&target, b"old binary").unwrap();
        let mut permissions = std::fs::metadata(&readonly).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&readonly, permissions).unwrap();

        let error = replace(&target, b"new binary").unwrap_err().to_string();

        let mut permissions = std::fs::metadata(&readonly).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&readonly, permissions).unwrap();

        assert!(
            error.contains(&readonly.display().to_string()),
            "the error must name the directory: {error}"
        );
    }

    #[test]
    fn replace_reports_a_directory_that_does_not_exist() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");
        let target = missing.join(BINARY);

        let error = replace(&target, b"new binary").unwrap_err().to_string();

        assert!(
            error.contains(&missing.display().to_string()),
            "the error must name the directory: {error}"
        );
    }

    #[test]
    fn repository_defaults_to_the_release_repository() {
        assert_eq!(repository(), DEFAULT_REPOSITORY);
    }

    async fn release_mirror(version: &str, archive: Vec<u8>, digest_line: String) -> String {
        use axum::{Router, routing::get};
        let base = format!("/team/repo/releases/download/v{version}");
        let archive_route = format!("{base}/{ASSET}");
        let checksums_route = format!("{base}/checksums.txt");
        let api_route = "/repos/team/repo/releases/latest";
        let tag = format!("{{\"tag_name\":\"v{version}\"}}");
        let router = Router::new()
            .route(&archive_route, get(move || async move { archive }))
            .route(&checksums_route, get(move || async move { digest_line }))
            .route(api_route, get(move || async move { tag }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn resolves_downloads_verifies_and_installs_a_release() {
        let archive = archive_containing(BINARY, b"the updated binary");
        let digest_line = format!("{}  {ASSET}\n", hex::encode(Sha256::digest(&archive)));
        let base = release_mirror("9.9.9", archive, digest_line).await;
        let http = client().unwrap();

        let latest = latest_version(&http, &base, "team/repo").await.unwrap();
        assert_eq!(latest, Version::parse("9.9.9").unwrap());
        assert!(latest > Version::current().unwrap());

        let downloaded = download(&http, &base, "team/repo", latest).await.unwrap();
        let binary = extract(&downloaded).unwrap();

        let dir = tempdir().unwrap();
        let target = dir.path().join(BINARY);
        std::fs::write(&target, b"the old binary").unwrap();
        replace(&target, &binary).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"the updated binary");
    }

    #[tokio::test]
    async fn a_tampered_archive_fails_verification_and_never_reaches_disk() {
        let archive = archive_containing(BINARY, b"the updated binary");
        let wrong_digest = format!("{}  {ASSET}\n", "0".repeat(64));
        let base = release_mirror("9.9.9", archive, wrong_digest).await;
        let http = client().unwrap();

        let error = download(&http, &base, "team/repo", Version::parse("9.9.9").unwrap())
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("checksum verification failed"),
            "unexpected error: {error}"
        );
    }
}
