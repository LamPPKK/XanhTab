use std::{
    fs,
    net::{IpAddr, SocketAddr},
    path::Path,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use url::Url;

const MAX_WIREGUARD_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_PROXY_URL_BYTES: u64 = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedProxy {
    pub url: Url,
    pub endpoint: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireGuardPolicy {
    pub ipv4_default: bool,
    pub ipv6_default: bool,
}

#[derive(Debug, Clone, Copy)]
enum SecretPermissions {
    OwnerOnly,
    GroupReadable,
}

pub fn validate_proxy_url(raw: &str, allow_credentials: bool) -> Result<ValidatedProxy> {
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_PROXY_URL_BYTES as usize {
        bail!("proxy URL must be non-empty and no larger than 8 KiB");
    }
    let url = Url::parse(value).context("proxy URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        bail!("proxy URL uses an unsupported scheme");
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        bail!("proxy URL must not contain a path, query, or fragment");
    }
    if !allow_credentials && (!url.username().is_empty() || url.password().is_some()) {
        bail!("managed proxy URL must not contain credentials");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("proxy URL must contain a host"))?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .context("proxy host must be a literal IP address")?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("proxy URL scheme does not provide a usable port"))?;
    Ok(ValidatedProxy {
        url,
        endpoint: SocketAddr::new(ip, port),
    })
}

pub fn validate_proxy_url_file(
    path: impl AsRef<Path>,
    expected_endpoint: Option<SocketAddr>,
    required_uid: Option<u32>,
) -> Result<ValidatedProxy> {
    let raw = read_secret_file(
        path.as_ref(),
        MAX_PROXY_URL_BYTES,
        SecretPermissions::GroupReadable,
        required_uid,
    )?;
    let proxy = validate_proxy_url(&raw, true)?;
    if expected_endpoint.is_some_and(|expected| expected != proxy.endpoint) {
        bail!("proxy URL endpoint does not match the configured kill-switch endpoint");
    }
    Ok(proxy)
}

pub fn validate_wireguard_config_file(
    path: impl AsRef<Path>,
    required_uid: Option<u32>,
) -> Result<WireGuardPolicy> {
    let raw = read_secret_file(
        path.as_ref(),
        MAX_WIREGUARD_CONFIG_BYTES,
        SecretPermissions::OwnerOnly,
        required_uid,
    )?;
    validate_wireguard_config(&raw)
}

pub fn validate_wireguard_config(raw: &str) -> Result<WireGuardPolicy> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        Interface,
        Peer,
    }

    let mut section = None;
    let mut interface_count = 0usize;
    let mut peer_count = 0usize;
    let mut interface_private_key = false;
    let mut interface_address = false;
    let mut table_off = false;
    let mut peer_public_key = false;
    let mut peer_allowed_ips = false;
    let mut peer_endpoint = false;
    let mut ipv4_default = false;
    let mut ipv6_default = false;

    let finish_peer =
        |peer_count: usize, public_key: bool, allowed_ips: bool, endpoint: bool| -> Result<()> {
            if peer_count > 0 && (!public_key || !allowed_ips || !endpoint) {
                bail!("every WireGuard peer requires PublicKey, AllowedIPs, and Endpoint");
            }
            Ok(())
        };

    for (index, raw_line) in raw.lines().enumerate() {
        let line = raw_line
            .split_once('#')
            .map_or(raw_line, |(content, _)| content)
            .trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            match line.to_ascii_lowercase().as_str() {
                "[interface]" => {
                    finish_peer(peer_count, peer_public_key, peer_allowed_ips, peer_endpoint)?;
                    interface_count += 1;
                    if interface_count != 1 || peer_count != 0 {
                        bail!("WireGuard config must contain one Interface section before peers");
                    }
                    section = Some(Section::Interface);
                }
                "[peer]" => {
                    if interface_count != 1 {
                        bail!("WireGuard Peer must follow the Interface section");
                    }
                    finish_peer(peer_count, peer_public_key, peer_allowed_ips, peer_endpoint)?;
                    peer_count += 1;
                    peer_public_key = false;
                    peer_allowed_ips = false;
                    peer_endpoint = false;
                    section = Some(Section::Peer);
                }
                _ => bail!("WireGuard config contains an unsupported section"),
            }
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid WireGuard line {}", index + 1))?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            bail!("WireGuard value on line {} must not be empty", index + 1);
        }
        match section {
            Some(Section::Interface) => match key.as_str() {
                "privatekey" => {
                    validate_wireguard_key(value, "PrivateKey")?;
                    if interface_private_key {
                        bail!("WireGuard Interface contains duplicate PrivateKey");
                    }
                    interface_private_key = true;
                }
                "address" => {
                    validate_cidr_list(value, "Interface Address")?;
                    interface_address = true;
                }
                "listenport" => validate_nonzero_u16(value, "ListenPort")?,
                "mtu" => validate_mtu(value)?,
                "table" => {
                    if table_off || !value.eq_ignore_ascii_case("off") {
                        bail!("WireGuard Interface must contain exactly one Table=off");
                    }
                    table_off = true;
                }
                "dns" | "preup" | "postup" | "predown" | "postdown" | "saveconfig" => {
                    bail!("WireGuard config contains a prohibited wg-quick directive")
                }
                _ => bail!("WireGuard Interface contains an unsupported directive"),
            },
            Some(Section::Peer) => match key.as_str() {
                "publickey" => {
                    validate_wireguard_key(value, "PublicKey")?;
                    if peer_public_key {
                        bail!("WireGuard Peer contains duplicate PublicKey");
                    }
                    peer_public_key = true;
                }
                "presharedkey" => validate_wireguard_key(value, "PresharedKey")?,
                "allowedips" => {
                    let defaults = validate_cidr_list(value, "AllowedIPs")?;
                    ipv4_default |= defaults.0;
                    ipv6_default |= defaults.1;
                    peer_allowed_ips = true;
                }
                "endpoint" => {
                    validate_wireguard_endpoint(value)?;
                    if peer_endpoint {
                        bail!("WireGuard Peer contains duplicate Endpoint");
                    }
                    peer_endpoint = true;
                }
                "persistentkeepalive" => validate_u16(value, "PersistentKeepalive")?,
                _ => bail!("WireGuard Peer contains an unsupported directive"),
            },
            None => bail!("WireGuard directive appears outside a section"),
        }
    }

    finish_peer(peer_count, peer_public_key, peer_allowed_ips, peer_endpoint)?;
    if interface_count != 1 || !interface_private_key || !interface_address || !table_off {
        bail!("WireGuard Interface requires PrivateKey, Address, and Table=off");
    }
    if peer_count == 0 || (!ipv4_default && !ipv6_default) {
        bail!("WireGuard config requires at least one full-tunnel default route");
    }
    Ok(WireGuardPolicy {
        ipv4_default,
        ipv6_default,
    })
}

fn validate_wireguard_key(value: &str, name: &str) -> Result<()> {
    let decoded = STANDARD
        .decode(value)
        .with_context(|| format!("WireGuard {name} is not valid base64"))?;
    if decoded.len() != 32 {
        bail!("WireGuard {name} must decode to 32 bytes");
    }
    Ok(())
}

fn validate_cidr_list(value: &str, name: &str) -> Result<(bool, bool)> {
    let mut any = false;
    let mut ipv4_default = false;
    let mut ipv6_default = false;
    for raw in value.split(',') {
        let cidr = raw.trim();
        let (address, prefix) = cidr
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("WireGuard {name} must use CIDR notation"))?;
        let address = address
            .parse::<IpAddr>()
            .with_context(|| format!("WireGuard {name} contains an invalid IP address"))?;
        let prefix = prefix
            .parse::<u8>()
            .with_context(|| format!("WireGuard {name} contains an invalid prefix"))?;
        match address {
            IpAddr::V4(address) if prefix <= 32 => {
                ipv4_default |= address.is_unspecified() && prefix == 0;
            }
            IpAddr::V6(address) if prefix <= 128 => {
                ipv6_default |= address.is_unspecified() && prefix == 0;
            }
            _ => bail!("WireGuard {name} prefix is outside the address-family range"),
        }
        any = true;
    }
    if !any {
        bail!("WireGuard {name} must not be empty");
    }
    Ok((ipv4_default, ipv6_default))
}

fn validate_u16(value: &str, name: &str) -> Result<()> {
    value
        .parse::<u16>()
        .with_context(|| format!("WireGuard {name} is invalid"))?;
    Ok(())
}

fn validate_nonzero_u16(value: &str, name: &str) -> Result<()> {
    let number = value
        .parse::<u16>()
        .with_context(|| format!("WireGuard {name} is invalid"))?;
    if number == 0 {
        bail!("WireGuard {name} must be greater than zero");
    }
    Ok(())
}

fn validate_mtu(value: &str) -> Result<()> {
    let mtu = value.parse::<u16>().context("WireGuard MTU is invalid")?;
    if !(576..=9_000).contains(&mtu) {
        bail!("WireGuard MTU is outside the supported range");
    }
    Ok(())
}

fn validate_wireguard_endpoint(value: &str) -> Result<()> {
    if !value.starts_with('[') && value.matches(':').count() != 1 {
        bail!("WireGuard Endpoint must be host:port or [IPv6]:port");
    }
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("WireGuard Endpoint must include a port"))?;
    if let Some(ipv6) = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        ipv6.parse::<std::net::Ipv6Addr>()
            .context("WireGuard Endpoint contains an invalid IPv6 address")?;
    } else if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("WireGuard Endpoint contains an invalid hostname");
    }
    validate_nonzero_u16(port, "Endpoint port")
}

fn read_secret_file(
    path: &Path,
    max_bytes: u64,
    permissions: SecretPermissions,
    required_uid: Option<u32>,
) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect secret file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("secret input must be a regular non-symlink file");
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        bail!("secret input has an invalid size");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if required_uid.is_some_and(|uid| metadata.uid() != uid) {
            bail!("secret input has an unexpected owner");
        }
        let mode = metadata.mode() & 0o777;
        let forbidden = match permissions {
            SecretPermissions::OwnerOnly => 0o077,
            SecretPermissions::GroupReadable => 0o027,
        };
        if mode & forbidden != 0 {
            bail!("secret input permissions are too broad");
        }
    }
    fs::read_to_string(path)
        .with_context(|| format!("failed to read secret file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn valid_wireguard() -> String {
        format!(
            "[Interface]\nPrivateKey = {KEY}\nAddress = 10.0.0.2/32\nTable = off\n\n[Peer]\nPublicKey = {KEY}\nAllowedIPs = 0.0.0.0/0, ::/0\nEndpoint = 192.0.2.1:51820\n"
        )
    }

    #[test]
    fn proxy_requires_literal_endpoint_and_no_url_suffix() {
        let proxy = validate_proxy_url("socks5h://user:pass@192.0.2.1:1080", true).unwrap();
        assert_eq!(proxy.endpoint, "192.0.2.1:1080".parse().unwrap());
        assert!(validate_proxy_url("socks5h://proxy.example:1080", true).is_err());
        assert!(validate_proxy_url("socks5h://192.0.2.1:1080/path", true).is_err());
        assert!(validate_proxy_url("socks5h://user:pass@127.0.0.1:9050", false).is_err());
    }

    #[test]
    fn wireguard_accepts_strict_full_tunnel_without_hooks() {
        assert_eq!(
            validate_wireguard_config(&valid_wireguard()).unwrap(),
            WireGuardPolicy {
                ipv4_default: true,
                ipv6_default: true,
            }
        );
    }

    #[test]
    fn wireguard_rejects_wg_quick_code_execution_directives() {
        for directive in [
            "DNS",
            "PreUp",
            "PostUp",
            "PreDown",
            "PostDown",
            "SaveConfig",
        ] {
            let config = valid_wireguard().replace(
                "Table = off",
                &format!("Table = off\n{directive} = /bin/sh -c id"),
            );
            assert!(validate_wireguard_config(&config).is_err(), "{directive}");
        }
    }

    #[test]
    fn wireguard_requires_table_off_and_a_default_route() {
        assert!(
            validate_wireguard_config(&valid_wireguard().replace("Table = off\n", "")).is_err()
        );
        assert!(
            validate_wireguard_config(&valid_wireguard().replace("0.0.0.0/0, ::/0", "10.0.0.0/8"))
                .is_err()
        );
        assert!(
            validate_wireguard_config(&valid_wireguard().replace("192.0.2.1:51820", "192.0.2.1:0"))
                .is_err()
        );
        assert!(
            validate_wireguard_config(
                &valid_wireguard().replace("Endpoint = 192.0.2.1:51820\n", "")
            )
            .is_err()
        );
    }
}
