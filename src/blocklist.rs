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

use anyhow::{Context, Result};
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
            if let Some(domain) = parse_hosts_line(&line?) {
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

fn parse_hosts_line(line: &str) -> Option<String> {
    let content = line.split('#').next()?.trim();
    if content.is_empty() {
        return None;
    }
    let parts: Vec<&str> = content.split_whitespace().collect();
    let candidate = if parts.len() > 1 && parts[0].parse::<std::net::IpAddr>().is_ok() {
        parts[1]
    } else {
        parts[0]
    };
    let host = normalize_host(candidate);
    if host.is_empty() || host == "localhost" || !host.contains('.') {
        return None;
    }
    Some(host)
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn compiles_hosts_and_matches_subdomains() {
        let mut source = NamedTempFile::new().unwrap();
        writeln!(source, "0.0.0.0 ads.example.com").unwrap();
        writeln!(source, "tracker.example.net # comment").unwrap();
        writeln!(source, "localhost").unwrap();
        let output = NamedTempFile::new().unwrap();
        compile_hosts(&[source.path()], output.path()).unwrap();
        let list = Blocklist::open(output.path()).unwrap();
        assert!(list.contains("pixel.ads.example.com"));
        assert!(list.contains("tracker.example.net"));
        assert!(!list.contains("example.org"));
        assert_eq!(list.len(), 2);
        assert_eq!(list.hits(), 2);
    }
}
