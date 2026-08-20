use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use fst::{Set, SetBuilder, Streamer};
use memmap2::{Mmap, MmapOptions};

#[derive(Clone, Default)]
pub struct Blocklist {
    set: Option<Arc<Set<Mmap>>>,
    hits: Arc<AtomicU64>,
}

impl Blocklist {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect blocklist {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("blocklist must be a regular non-symlink file");
        }
        let set = open_fst(path)?;
        Ok(Self {
            set: Some(Arc::new(set)),
            hits: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn contains(&self, host: &str) -> bool {
        let Some(set) = &self.set else { return false };
        let host = normalize_host(host);
        let labels: Vec<&str> = host.split('.').collect();
        for index in 0..labels.len() {
            if set.contains(labels[index..].join(".")) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.set.as_ref().map_or(0, |set| set.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn validate_fst_file(path: impl AsRef<Path>) -> Result<usize> {
    let set = open_fst(path.as_ref())?;
    Ok(set.len())
}

pub fn compile_hosts(inputs: &[impl AsRef<Path>], output: impl AsRef<Path>) -> Result<usize> {
    let mut domains = BTreeSet::new();
    for input in inputs {
        let input = input.as_ref();
        if !input.exists() {
            continue;
        }
        let reader = BufReader::new(
            File::open(input).with_context(|| format!("failed to read {}", input.display()))?,
        );
        for line in reader.lines() {
            for domain in parse_hosts_line(&line?) {
                domains.insert(domain);
            }
        }
    }
    if let Some(parent) = output.as_ref().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = output.as_ref().with_extension("fst.tmp");
    let mut builder = SetBuilder::new(File::create(&temporary)?)?;
    for domain in &domains {
        builder.insert(domain)?;
    }
    builder.finish()?;
    std::fs::rename(temporary, output.as_ref())?;
    Ok(domains.len())
}

pub fn merge_hosts_with_base(
    base_fst: impl AsRef<Path>,
    inputs: &[impl AsRef<Path>],
    output: impl AsRef<Path>,
) -> Result<usize> {
    let base_fst = base_fst.as_ref();
    let output = output.as_ref();
    let base = open_fst(base_fst)
        .with_context(|| format!("invalid base blocklist {}", base_fst.display()))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }

    let custom_fst = output.with_extension("custom.fst.tmp");
    let merged_fst = output.with_extension("fst.tmp");
    let result = (|| -> Result<usize> {
        compile_hosts(inputs, &custom_fst)?;
        let custom = open_fst(&custom_fst)?;
        let mut union = base.op().add(&custom).union();
        let mut builder = SetBuilder::new(File::create(&merged_fst)?)?;
        let mut count = 0usize;
        while let Some(domain) = union.next() {
            builder.insert(domain)?;
            count += 1;
        }
        builder.finish()?;
        fs::rename(&merged_fst, output)?;
        Ok(count)
    })();
    let _ = fs::remove_file(&custom_fst);
    if result.is_err() {
        let _ = fs::remove_file(&merged_fst);
    }
    result
}

fn open_fst(path: &Path) -> Result<Set<Mmap>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect blocklist {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("blocklist must be a regular non-symlink file");
    }
    let file =
        File::open(path).with_context(|| format!("failed to open blocklist {}", path.display()))?;
    // SAFETY: the map is read-only, and production updates replace the file atomically.
    let mmap = unsafe { MmapOptions::new().map(&file) }?;
    Set::new(mmap).context("invalid FST blocklist")
}

pub fn validate_hosts_file(path: impl AsRef<Path>) -> Result<usize> {
    let path = path.as_ref();
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?,
    );
    let mut domains = BTreeSet::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let content = line.split('#').next().unwrap_or_default().trim();
        if content.is_empty() {
            continue;
        }
        let parts: Vec<&str> = content.split_whitespace().collect();
        let candidates = if parts
            .first()
            .is_some_and(|part| part.parse::<std::net::IpAddr>().is_ok())
        {
            &parts[1..]
        } else {
            &parts[..]
        };
        if candidates.is_empty() {
            bail!(
                "{}:{} does not contain a hostname",
                path.display(),
                index + 1
            );
        }
        for candidate in candidates {
            let Some(domain) = normalize_domain(candidate) else {
                bail!(
                    "{}:{} contains an invalid hostname",
                    path.display(),
                    index + 1
                );
            };
            domains.insert(domain);
        }
    }
    Ok(domains.len())
}

fn parse_hosts_line(line: &str) -> Vec<String> {
    let content = line.split('#').next().unwrap_or_default().trim();
    if content.is_empty() {
        return Vec::new();
    }
    let parts: Vec<&str> = content.split_whitespace().collect();
    let candidates = if parts
        .first()
        .is_some_and(|part| part.parse::<std::net::IpAddr>().is_ok())
    {
        &parts[1..]
    } else {
        &parts[..]
    };
    candidates
        .iter()
        .filter_map(|candidate| normalize_domain(candidate))
        .collect()
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn normalize_domain(candidate: &str) -> Option<String> {
    let host = normalize_host(candidate);
    if host.is_empty()
        || host == "localhost"
        || !host.contains('.')
        || host.len() > 253
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return None;
    }
    let valid = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    valid.then_some(host)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn compiles_hosts_and_matches_subdomains() {
        let mut source = NamedTempFile::new().unwrap();
        writeln!(source, "0.0.0.0 ads.example.com cdn.example.com").unwrap();
        writeln!(source, "tracker.example.net # comment").unwrap();
        writeln!(source, "localhost").unwrap();
        let output = NamedTempFile::new().unwrap();
        compile_hosts(&[source.path()], output.path()).unwrap();
        let list = Blocklist::open(output.path()).unwrap();
        assert!(list.contains("pixel.ads.example.com"));
        assert!(list.contains("tracker.example.net"));
        assert!(!list.contains("example.org"));
        assert!(list.contains("cdn.example.com"));
        assert_eq!(list.len(), 3);
        assert_eq!(list.hits(), 3);
    }

    #[test]
    fn strict_hosts_validation_rejects_non_domains() {
        let mut source = NamedTempFile::new().unwrap();
        writeln!(source, "0.0.0.0 ads.example.com").unwrap();
        writeln!(source, "../etc/passwd").unwrap();
        assert!(validate_hosts_file(source.path()).is_err());
    }

    #[test]
    fn strict_hosts_validation_accepts_comments_and_aliases() {
        let mut source = NamedTempFile::new().unwrap();
        writeln!(source, "# local additions").unwrap();
        writeln!(source, "0.0.0.0 ads.example.com cdn.example.net # aliases").unwrap();
        writeln!(source, "pixel.example.org.").unwrap();
        assert_eq!(validate_hosts_file(source.path()).unwrap(), 3);
    }

    #[test]
    fn streaming_union_merges_base_and_custom_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let base_source = root.path().join("base-hosts.txt");
        let custom_source = root.path().join("custom-hosts.txt");
        let base_fst = root.path().join("base.fst");
        let merged_fst = root.path().join("merged.fst");
        std::fs::write(
            &base_source,
            "0.0.0.0 ads.example.com\n0.0.0.0 tracker.example.net\n",
        )
        .unwrap();
        std::fs::write(
            &custom_source,
            "0.0.0.0 tracker.example.net custom.example.org\n",
        )
        .unwrap();
        compile_hosts(&[base_source], &base_fst).unwrap();

        assert_eq!(
            merge_hosts_with_base(&base_fst, &[custom_source], &merged_fst).unwrap(),
            3
        );
        assert_eq!(validate_fst_file(&merged_fst).unwrap(), 3);
        let merged = Blocklist::open(&merged_fst).unwrap();
        assert!(merged.contains("pixel.ads.example.com"));
        assert!(merged.contains("tracker.example.net"));
        assert!(merged.contains("custom.example.org"));
    }

    #[test]
    fn fst_validation_rejects_malformed_input() {
        let source = NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"not-an-fst").unwrap();
        assert!(validate_fst_file(source.path()).is_err());
    }
}
