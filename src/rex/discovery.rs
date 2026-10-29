//! Read-only host discovery through a server's authoritative host list.
use super::{endpoint::Endpoint, transport::ControlConnection};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_SERVERS: usize = 16;
const MAX_HOSTS: usize = 1024;
const MAX_ADDRESSES: usize = 32;

/// A saved host with an address supported by the native transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub name: String,
    pub endpoint: String,
}

/// Read saved hosts without attaching sessions or changing the server's host list.
///
/// Call only on a background worker: the native transport bounds handshake,
/// response time and message size. This reads the selected server's list, not
/// the CLI's merged home/local cache. Bare host names use Rex's documented first
/// candidate, HTTPS on port 443; a supplied port is retained. The CLI's later
/// HTTPS/HTTP port-7297 fallback candidates are not attempted here. Unsupported
/// account-selection URLs and malformed addresses are omitted.
///
/// # Errors
/// Reports connection/request failures or a malformed or oversized host list.
pub fn discover_servers(endpoint: &str) -> Result<Vec<DiscoveredServer>> {
    let mut connection = ControlConnection::connect(endpoint)?;
    parse_hosts(&connection.call("hosts.list", json!({}))?)
}

fn normalize_endpoint(value: &str) -> Option<String> {
    // Rex documents HTTPS as the initial candidate for a bare host. Never infer
    // an HTTP downgrade, an account identity, or a path from saved host data.
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        if value.contains(['/', '\\', '@', '?', '#']) {
            return None;
        }
        format!("https://{value}")
    };
    match Endpoint::parse(&candidate).ok()? {
        Endpoint::Unix(path) => {
            let path: std::path::PathBuf = path.components().collect();
            Some(format!("unix://{}", path.display()))
        }
        Endpoint::Network(url) => Some(url.to_string()),
    }
}

fn parse_hosts(value: &Value) -> Result<Vec<DiscoveredServer>> {
    let hosts = value.get("hosts").context("Rex host list lacks hosts")?;
    if hosts.is_null() {
        return Ok(Vec::new());
    }
    let hosts = hosts.as_array().context("invalid Rex host list")?;
    ensure!(hosts.len() <= MAX_HOSTS, "Rex host list exceeds 1024 hosts");
    let mut endpoints = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut servers = Vec::new();
    for host in hosts {
        let name = host["label"].as_str().context("Rex host lacks a label")?;
        ensure!(
            !name.is_empty()
                && name.chars().count() <= 64
                && name.trim() == name
                && !name.chars().any(char::is_control),
            "invalid Rex host label"
        );
        let addresses = host.get("endpoints").context("Rex host lacks endpoints")?;
        if addresses.is_null() {
            continue;
        }
        let addresses = addresses.as_array().context("invalid Rex host endpoints")?;
        ensure!(
            addresses.len() <= MAX_ADDRESSES,
            "Rex host has too many endpoints"
        );
        let mut endpoint = None;
        for address in addresses {
            let address = address.as_str().context("invalid Rex host endpoint")?;
            if endpoint.is_none() {
                endpoint = normalize_endpoint(address);
            }
        }
        let Some(endpoint) = endpoint else {
            continue;
        };
        if endpoints.contains(&endpoint) || names.contains(&name.to_lowercase()) {
            continue;
        }
        endpoints.insert(endpoint.clone());
        names.insert(name.to_lowercase());
        servers.push(DiscoveredServer {
            name: name.to_owned(),
            endpoint,
        });
        if servers.len() == MAX_SERVERS {
            break;
        }
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::{normalize_endpoint, parse_hosts};
    use anyhow::Result;
    use serde_json::json;

    #[test]
    fn reads_supported_addresses_in_host_order_with_secure_bare_host_default() -> Result<()> {
        let servers = parse_hosts(&json!({"hosts":[
            {"label":"Studio","endpoints":["studio.tail.example","https://STUDIO.example:443"]},
            {"label":"Local","endpoints":["unix:///tmp/./rex.sock"]},
            {"label":"Bare","endpoints":["192.168.1.2:7297"]},
            {"label":"Empty","endpoints":null}
        ],"hash":"version","diagnostics":[]}))?;
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "Studio");
        assert_eq!(servers[0].endpoint, "wss://studio.tail.example/");
        assert_eq!(servers[1].endpoint, "unix:///tmp/rex.sock");
        assert_eq!(servers[2].endpoint, "wss://192.168.1.2:7297/");
        Ok(())
    }

    #[test]
    fn bare_origins_preserve_ports_and_bracketed_ipv6_without_accepting_url_suffixes() {
        for (value, expected) in [
            ("STUDIO.example:443", "wss://studio.example/"),
            ("studio.example:7297", "wss://studio.example:7297/"),
            ("[::1]", "wss://[::1]/"),
            ("[::1]:7297", "wss://[::1]:7297/"),
        ] {
            assert_eq!(normalize_endpoint(value).as_deref(), Some(expected));
        }
        for value in [
            "user@studio.example",
            "studio.example/?token=secret",
            "studio.example/path",
            "studio.example#fragment",
            "studio.example\\path",
            "::1",
            "",
            "studio.example\n",
        ] {
            assert_eq!(normalize_endpoint(value), None);
        }
    }

    #[test]
    fn aliases_and_case_insensitive_labels_are_deduplicated() -> Result<()> {
        let servers = parse_hosts(&json!({"hosts":[
            {"label":"Studio","endpoints":["https://example.com"]},
            {"label":"Alias","endpoints":["wss://example.com/"]},
            {"label":"STUDIO","endpoints":["https://another.example"]}
        ]}))?;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "Studio");
        Ok(())
    }

    #[test]
    fn malformed_lists_fail_and_unsafe_urls_are_not_returned() -> Result<()> {
        for value in [
            json!({}),
            json!({"hosts":{}}),
            json!({"hosts":[{"endpoints":[]}]}),
            json!({"hosts":[{"label":"bad\nlabel","endpoints":[]}]}),
            json!({"hosts":[{"label":"Host","endpoints":[42]}]}),
        ] {
            assert!(parse_hosts(&value).is_err());
        }
        assert_eq!(parse_hosts(&json!({"hosts":null}))?, []);
        assert_eq!(
            parse_hosts(&json!({"hosts":[{"label":"Host","endpoints":[
                "https://user:secret@example.com", "http://example.com/api/control",
                "http://example.com/?token=secret", "unix://relative"
            ]}]}))?,
            []
        );
        Ok(())
    }

    #[test]
    fn bounds_host_input_and_connection_output() -> Result<()> {
        let hosts: Vec<_> = (0..20)
            .map(|index| json!({"label":format!("host{index}"),"endpoints":[format!("https://host{index}.example")]}))
            .collect();
        assert_eq!(parse_hosts(&json!({"hosts":hosts}))?.len(), 16);
        assert!(parse_hosts(&json!({"hosts":vec![json!({}); 1025]})).is_err());
        assert!(
            parse_hosts(
                &json!({"hosts":[{"label":"Host","endpoints":vec!["https://example.com";33]}]})
            )
            .is_err()
        );
        Ok(())
    }
}
