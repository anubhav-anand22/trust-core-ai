//! Finding the Host on the LAN from its code (mDNS / DNS-SD, no server).
//!
//! The Host advertises `_trustcore._tcp` with the code's public tag and its
//! port. A Worker browses, keeps the Hosts whose tag matches what the user typed,
//! and pairs with the first one. Only the tag is ever broadcast: the secret
//! part of the code never leaves the two devices (it only feeds SPAKE2).
//!
//! A network that blocks multicast (some guest Wi-Fi, some VPNs) finds nothing.
//! The UI then offers "enter the Host's address" instead.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::{MeshError, Result, PROTOCOL_VERSION};

pub const SERVICE_TYPE: &str = "_trustcore._tcp.local.";

fn mdns_err(e: mdns_sd::Error) -> MeshError {
    MeshError::Protocol(format!("LAN discovery: {e}"))
}

/// The Host's announcement. Withdrawn when dropped.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Announce this Host. `instance` must be unique on the LAN (the device id is).
    pub fn start(instance: &str, port: u16, tag: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(mdns_err)?;
        let proto = PROTOCOL_VERSION.to_string();
        let props = [("tag", tag), ("proto", proto.as_str())];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &format!("{instance}.local."),
            "",
            port,
            &props[..],
        )
        .map_err(mdns_err)?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(mdns_err)?;
        tracing::info!(port, tag, "mesh: advertising on the LAN");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Addresses of Hosts advertising `tag`, waiting up to `wait` for the first.
/// Empty means none was found in time.
pub async fn find_hosts(tag: &str, wait: Duration) -> Result<Vec<SocketAddr>> {
    let tag = tag.to_string();
    tokio::task::spawn_blocking(move || {
        let daemon = ServiceDaemon::new().map_err(mdns_err)?;
        let events = daemon.browse(SERVICE_TYPE).map_err(mdns_err)?;
        let deadline = Instant::now() + wait;
        let mut found = Vec::new();
        while found.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            let Ok(event) = events.recv_timeout(left) else { break };
            if let ServiceEvent::ServiceResolved(info) = event {
                if info.get_property_val_str("tag") != Some(tag.as_str()) {
                    continue;
                }
                let port = info.get_port();
                found.extend(
                    info.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), port)),
                );
            }
        }
        let _ = daemon.stop_browse(SERVICE_TYPE);
        let _ = daemon.shutdown();
        Ok(found)
    })
    .await
    .map_err(|e| MeshError::Protocol(format!("LAN discovery task: {e}")))?
}
