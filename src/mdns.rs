use std::sync::mpsc::{Receiver as StdReceiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::config::normalize_server_url;
use crate::constants::{DEFAULT_SERVER_PATH, SENDSPIN_SERVER_SERVICE_TYPE};
use crate::state::{ConnectionState, DiscoveredServer, MdnsCommand, SharedState};

pub(crate) fn mdns_thread_main(shared: Arc<SharedState>, command_rx: StdReceiver<MdnsCommand>) {
    let Ok(mdns) = ServiceDaemon::new() else {
        return;
    };

    let mut browse_rx = match mdns.browse(SENDSPIN_SERVER_SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(_) => {
            let _ = mdns.shutdown();
            return;
        }
    };

    let mut discovered_by_id: std::collections::BTreeMap<String, DiscoveredServer> =
        std::collections::BTreeMap::new();

    loop {
        match command_rx.try_recv() {
            Ok(MdnsCommand::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Ok(MdnsCommand::Refresh) => {
                let _ = mdns.stop_browse(SENDSPIN_SERVER_SERVICE_TYPE);
                discovered_by_id.clear();
                shared.set_discovered_servers(Vec::new());

                if let Ok(new_receiver) = mdns.browse(SENDSPIN_SERVER_SERVICE_TYPE) {
                    browse_rx = new_receiver;
                }
                continue;
            }
            Err(TryRecvError::Empty) => {}
        }

        let Ok(event) = browse_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };

        match event {
            ServiceEvent::ServiceResolved(info) => {
                if let Some(server) = discovered_server_from_mdns(&info) {
                    discovered_by_id.insert(server.id.clone(), server);
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                discovered_by_id.remove(&fullname);
            }
            _ => {}
        }

        let mut servers: Vec<DiscoveredServer> = discovered_by_id.values().cloned().collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name).then(a.url.cmp(&b.url)));
        shared.set_discovered_servers(servers.clone());

        let configured_server_url = shared.configured_server_url();
        let configured_server_url =
            normalize_server_url(&configured_server_url).unwrap_or_default();
        let should_auto_switch = shared.connection_state() != ConnectionState::Connected;
        if should_auto_switch {
            if let Some(desired_server_url) =
                auto_selected_server_url(&configured_server_url, &servers)
            {
                if configured_server_url != desired_server_url {
                    shared.request_server_switch(desired_server_url);
                }
            }
        }
    }

    let _ = mdns.stop_browse(SENDSPIN_SERVER_SERVICE_TYPE);
    if let Ok(shutdown_rx) = mdns.shutdown() {
        let _ = shutdown_rx.recv_timeout(Duration::from_secs(1));
    }
    shared.set_discovered_servers(Vec::new());
}

fn discovered_server_from_mdns(info: &mdns_sd::ServiceInfo) -> Option<DiscoveredServer> {
    let fullname = info.get_fullname().to_string();
    let instance_suffix = format!(".{SENDSPIN_SERVER_SERVICE_TYPE}");
    let name = fullname
        .strip_suffix(&instance_suffix)
        .unwrap_or(fullname.as_str())
        .to_string();

    let mut addresses: Vec<_> = info.get_addresses().iter().copied().collect();
    addresses.sort_by_key(|addr| if addr.is_ipv4() { 0_u8 } else { 1_u8 });
    let host = addresses.first()?.to_string();
    let host_fmt = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };

    let mut path = info
        .get_property_val_str("path")
        .unwrap_or(DEFAULT_SERVER_PATH)
        .to_string();
    if path.is_empty() {
        path = DEFAULT_SERVER_PATH.to_string();
    } else if !path.starts_with('/') {
        path = format!("/{path}");
    }

    let raw_url = format!("ws://{host_fmt}:{}{}", info.get_port(), path);
    let url = normalize_server_url(&raw_url)?;

    Some(DiscoveredServer {
        id: fullname,
        name,
        url,
    })
}

fn auto_selected_server_url(
    configured_server_url: &str,
    discovered_servers: &[DiscoveredServer],
) -> Option<String> {
    if discovered_servers.is_empty() {
        return None;
    }

    let normalized_configured = normalize_server_url(configured_server_url);
    if let Some(preferred_url) = normalized_configured {
        if discovered_servers
            .iter()
            .any(|entry| entry.url == preferred_url)
        {
            return Some(preferred_url);
        }
    }

    discovered_servers.first().map(|entry| entry.url.clone())
}

#[cfg(test)]
mod tests {
    use super::auto_selected_server_url;
    use crate::state::DiscoveredServer;

    fn server(id: &str, name: &str, url: &str) -> DiscoveredServer {
        DiscoveredServer {
            id: id.to_string(),
            name: name.to_string(),
            url: url.to_string(),
        }
    }

    #[test]
    fn auto_select_prefers_configured_server_when_available() {
        let servers = vec![
            server("1", "A", "ws://a.local:8927/sendspin"),
            server("2", "B", "ws://b.local:8927/sendspin"),
        ];

        let selected = auto_selected_server_url("ws://b.local:8927/sendspin", &servers);
        assert_eq!(selected.as_deref(), Some("ws://b.local:8927/sendspin"));
    }

    #[test]
    fn auto_select_falls_back_to_first_discovered_server() {
        let servers = vec![
            server("1", "A", "ws://a.local:8927/sendspin"),
            server("2", "B", "ws://b.local:8927/sendspin"),
        ];

        let selected = auto_selected_server_url("ws://missing.local:8927/sendspin", &servers);
        assert_eq!(selected.as_deref(), Some("ws://a.local:8927/sendspin"));
    }

    #[test]
    fn auto_select_returns_none_when_no_servers() {
        let selected = auto_selected_server_url("ws://a.local:8927/sendspin", &[]);
        assert!(selected.is_none());
    }
}
