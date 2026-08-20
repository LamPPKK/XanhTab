use std::{
    collections::BTreeSet,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use fst::{Set, SetBuilder};
use memmap2::{Mmap, MmapOptions};

#[derive(Clone, Default)]
pub struct Blocklist {
    set: Option<Arc<Set<Mmap>>>,
    hits: Arc<AtomicU64>,
}

impl Blocklist {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let file = File::open(path)
            .with_context(|| format!("failed to open blocklist {}", path.display()))?;
        // SAFETY: the map is read-only, and production updates replace the file atomically.
        let mmap = unsafe { MmapOptions::new().map(&file) }?;
        let set = Set::new(mmap).context("invalid FST blocklist")?;
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
}
