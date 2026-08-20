use std::{
    collections::HashSet,
    fs::{self, File},
    path::Path,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use url::Url;
use uuid::Uuid;

use crate::{blocklist::validate_hosts_file, config::Config, model::StreamProfile};

const MAX_BOOKMARKS: usize = 500;
const MAX_BLOCKLIST_SOURCES: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteConfigDocument {
    schema_version: u8,
    session: RemoteSessionConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSessionConfig {
    initial_url: Option<Url>,
    initial_profile: Option<StreamProfile>,
    auto_burn_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BookmarksDocument {
    schema_version: u8,
    bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bookmark {
    id: Uuid,
    title: String,
    url: Url,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlocklistMetadataDocument {
    schema_version: u8,
    generated_at: DateTime<Utc>,
    entry_count: u64,
    sources: Vec<BlocklistSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlocklistSource {
    name: String,
    file: String,
    url: Url,
    revision: String,
    sha256: String,
    license: String,
    license_url: Url,
    redistribution: RedistributionStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RedistributionStatus {
    Reviewed,
    ExternalFetchOnly,
}

pub fn validate_public_config_dir(directory: impl AsRef<Path>) -> Result<()> {
    let directory = directory.as_ref();
    if !directory.is_dir() || directory.is_symlink() {
        bail!(
            "public config directory is unavailable or unsafe: {}",
            directory.display()
        );
    }

    validate_optional_json(directory.join("config.json"), validate_remote_config)?;
    validate_optional_json(directory.join("bookmarks.json"), validate_bookmarks)?;
    validate_optional_json(
        directory.join("blocklist-metadata.json"),
        validate_blocklist_metadata,
    )?;

    let custom_hosts = directory.join("custom_hosts.txt");
    if path_exists_without_following(&custom_hosts)? {
        require_regular_file(&custom_hosts)?;
        validate_hosts_file(&custom_hosts)?;
    }
    Ok(())
}

pub fn apply_public_config_file(config: &mut Config, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    require_regular_file(path)?;
    let file = File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let document = parse_remote_config(file)
        .with_context(|| format!("invalid public config {}", path.display()))?;
    let session = &document.session;
    if let Some(url) = &session.initial_url {
        config.session.initial_url = url.to_string();
    }
    if let Some(profile) = session.initial_profile {
        config.session.initial_profile = profile;
    }
    if let Some(seconds) = session.auto_burn_seconds {
        config.session.auto_burn_seconds = seconds;
    }
    config.validate()
}

fn validate_optional_json<T>(path: impl AsRef<Path>, validate: T) -> Result<()>
where
    T: FnOnce(File) -> Result<()>,
{
    let path = path.as_ref();
    if !path_exists_without_following(path)? {
        return Ok(());
    }
    require_regular_file(path)?;
    let file = File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    validate(file).with_context(|| format!("invalid public config {}", path.display()))
}

fn path_exists_without_following(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn require_regular_file(path: &Path) -> Result<()> {
    if path.is_symlink() || !path.is_file() {
        bail!(
            "public config path is not a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_remote_config(file: File) -> Result<()> {
    parse_remote_config(file).map(|_| ())
}

fn parse_remote_config(file: File) -> Result<RemoteConfigDocument> {
    let document: RemoteConfigDocument = serde_json::from_reader(file)?;
    require_schema_v1(document.schema_version)?;
    let session = &document.session;
    if session.initial_url.is_none()
        && session.initial_profile.is_none()
        && session.auto_burn_seconds.is_none()
    {
        bail!("session must contain at least one reviewed setting");
    }
    if let Some(url) = &session.initial_url {
        let internal_home = url.as_str() == "xanhtab://home";
        let web_url = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none();
        if !internal_home && !web_url {
            bail!("initial_url must be HTTP(S) without credentials or xanhtab://home");
        }
    }
    if let Some(seconds) = session.auto_burn_seconds
        && seconds != 0
        && !(60..=86_400).contains(&seconds)
    {
        bail!("auto_burn_seconds must be zero or between 60 and 86400");
    }
    Ok(document)
}

fn validate_bookmarks(file: File) -> Result<()> {
    let document: BookmarksDocument = serde_json::from_reader(file)?;
    require_schema_v1(document.schema_version)?;
    if document.bookmarks.len() > MAX_BOOKMARKS {
        bail!("bookmarks exceeds the {MAX_BOOKMARKS}-item limit");
    }
    let mut ids = HashSet::with_capacity(document.bookmarks.len());
    for bookmark in document.bookmarks {
        if !ids.insert(bookmark.id) {
            bail!("bookmark IDs must be unique");
        }
        let title_length = bookmark.title.chars().count();
        if !(1..=256).contains(&title_length) {
            bail!("bookmark title must contain between 1 and 256 characters");
        }
        if bookmark.url.as_str().len() > 4096
            || !matches!(bookmark.url.scheme(), "http" | "https")
            || bookmark.url.host_str().is_none()
            || !bookmark.url.username().is_empty()
            || bookmark.url.password().is_some()
        {
            bail!("bookmark URL must be a credential-free HTTP(S) URL");
        }
    }
    Ok(())
}

fn validate_blocklist_metadata(file: File) -> Result<()> {
    let document: BlocklistMetadataDocument = serde_json::from_reader(file)?;
    require_schema_v1(document.schema_version)?;
    let _captured_at = document.generated_at;
    let _entry_count = document.entry_count;
    if document.sources.is_empty() || document.sources.len() > MAX_BLOCKLIST_SOURCES {
        bail!("blocklist metadata must contain between 1 and {MAX_BLOCKLIST_SOURCES} sources");
    }
    let mut names = HashSet::with_capacity(document.sources.len());
    let mut files = HashSet::with_capacity(document.sources.len());
    for source in document.sources {
        if source.name.is_empty()
            || source.name.chars().count() > 128
            || !source
                .name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || source.name.ends_with(' ')
            || !source.name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'_' | b'-')
            })
            || !names.insert(source.name.clone())
        {
            bail!("blocklist source names must be non-empty, bounded, and unique");
        }
        if source.file.is_empty()
            || source.file.len() > 128
            || !source
                .file
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !source
                .file
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || !files.insert(source.file.clone())
        {
            bail!("blocklist source filenames must be safe, bounded, and unique");
        }
        if source.url.scheme() != "https"
            || source.url.host_str().is_none()
            || !source.url.username().is_empty()
            || source.url.password().is_some()
        {
            bail!("blocklist source URL must be credential-free HTTPS");
        }
        if source.revision.is_empty()
            || source.revision.len() > 128
            || !source
                .revision
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !source
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
        {
            bail!("blocklist source revision is invalid");
        }
        if source.sha256.len() != 64
            || !source
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("blocklist source checksum must be lowercase SHA-256");
        }
        if source.license.is_empty()
            || source.license.len() > 128
            || source.license.starts_with(' ')
            || source.license.ends_with(' ')
            || !source
                .license
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
        {
            bail!("blocklist source license identifier is invalid");
        }
        if source.license_url.scheme() != "https"
            || source.license_url.host_str().is_none()
            || !source.license_url.username().is_empty()
            || source.license_url.password().is_some()
        {
            bail!("blocklist source license URL must be credential-free HTTPS");
        }
        let _redistribution = source.redistribution;
    }
    Ok(())
}

fn require_schema_v1(version: u8) -> Result<()> {
    if version != 1 {
        bail!("schema_version must be 1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn validates_complete_public_config_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote-config");
        validate_public_config_dir(root).unwrap();
    }

    #[test]
    fn rejects_unknown_or_secret_remote_config_fields() {
        let directory = tempdir().unwrap();
        let mut file = File::create(directory.path().join("config.json")).unwrap();
        write!(
            file,
            r#"{{"schema_version":1,"session":{{"initial_profile":"720p15"}},"proxy_password":"secret"}}"#
        )
        .unwrap();
        assert!(validate_public_config_dir(directory.path()).is_err());
    }

    #[test]
    fn rejects_duplicate_bookmark_ids() {
        let directory = tempdir().unwrap();
        let mut file = File::create(directory.path().join("bookmarks.json")).unwrap();
        write!(
            file,
            r#"{{"schema_version":1,"bookmarks":[{{"id":"5dbcd6bc-2531-44d5-a2b8-cc9dfbb2d28b","title":"One","url":"https://example.com"}},{{"id":"5dbcd6bc-2531-44d5-a2b8-cc9dfbb2d28b","title":"Two","url":"https://example.org"}}]}}"#
        )
        .unwrap();
        assert!(validate_public_config_dir(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_broken_symlink_in_an_optional_slot() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        symlink("missing.json", directory.path().join("bookmarks.json")).unwrap();
        assert!(validate_public_config_dir(directory.path()).is_err());
    }

    #[test]
    fn applies_only_reviewed_session_defaults() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote-config/config.json");
        let mut config = Config::default();
        apply_public_config_file(&mut config, path).unwrap();
        assert_eq!(config.session.initial_url, "https://example.com/start");
        assert_eq!(config.session.initial_profile, StreamProfile::Hd15);
        assert_eq!(config.session.auto_burn_seconds, 600);
        assert!(!config.browser.enabled);
        assert!(!config.network.enabled);
        assert!(config.server.tls_cert.is_none());
    }
}
