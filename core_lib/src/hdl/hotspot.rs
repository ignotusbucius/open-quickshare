//! Hosting a Wi-Fi network for the send-side bandwidth upgrade (dynamic role
//! switch): when the phone and PC share no LAN, the phone asks the *sender* to
//! host the upgraded medium — exactly what Quick Share for Windows does. We
//! bring up a NetworkManager hotspot (`shared` mode gives us DHCP/NAT at the
//! gateway address), hand its credentials to the phone over the BLE channel,
//! and tear it down when the transfer ends.
use std::net::Ipv4Addr;

use rand::Rng;
use tokio::process::Command;

const CON_NAME: &str = "packet-qs-hotspot";
/// Fixed listener port on the hotspot, so a firewall rule can be added once:
/// `sudo firewall-cmd --zone=nm-shared --add-port=61812/tcp`
pub const HOTSPOT_TCP_PORT: u16 = 61812;

/// A live hotspot; dropping it tears the network down (and NetworkManager
/// then reconnects the interface to its previous network).
pub struct HotspotGuard {
    pub ssid: String,
    pub password: String,
    pub gateway: Ipv4Addr,
    pub frequency: i32,
}

impl std::fmt::Debug for HotspotGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the password.
        f.debug_struct("HotspotGuard")
            .field("ssid", &self.ssid)
            .field("gateway", &self.gateway)
            .field("frequency", &self.frequency)
            .finish_non_exhaustive()
    }
}

impl Drop for HotspotGuard {
    fn drop(&mut self) {
        // Best-effort: `delete` also deactivates an active connection.
        let _ = std::process::Command::new("nmcli")
            .args(["connection", "delete", CON_NAME])
            .output();
        info!("Hotspot: '{}' torn down", self.ssid);
    }
}

const JOIN_CON_NAME: &str = "packet-qs-join";

/// A temporary membership of a peer-hosted network (the phone's Wi-Fi Direct
/// group / hotspot); dropping it leaves that network and restores the previous
/// Wi-Fi connection.
pub struct JoinGuard {
    ssid: String,
    prior: Option<String>,
}

impl std::fmt::Debug for JoinGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinGuard")
            .field("ssid", &self.ssid)
            .finish_non_exhaustive()
    }
}

impl Drop for JoinGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("nmcli")
            .args(["connection", "delete", JOIN_CON_NAME])
            .output();
        if let Some(prior) = &self.prior {
            let _ = std::process::Command::new("nmcli")
                .args(["connection", "up", prior])
                .output();
        }
        info!("Join: left '{}'", self.ssid);
    }
}

/// Joins a peer-hosted Wi-Fi network (the phone's Wi-Fi Direct group appears
/// as a WPA2 AP) and waits for an address. Takes the Wi-Fi interface off its
/// current network for the duration; the guard restores it on drop.
pub async fn join_wifi(ssid: &str, password: &str) -> Result<JoinGuard, anyhow::Error> {
    let devs = nmcli(&["-t", "-f", "DEVICE,TYPE", "device"]).await?;
    let ifname = devs
        .lines()
        .find_map(|l| l.strip_suffix(":wifi").map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("no Wi-Fi interface"))?;

    // Remember the current Wi-Fi connection so it can be restored afterwards.
    let suffix = format!(":{ifname}");
    let prior = nmcli(&["-t", "-f", "NAME,DEVICE", "connection", "show", "--active"])
        .await
        .ok()
        .and_then(|out| {
            out.lines()
                .find_map(|l| l.strip_suffix(suffix.as_str()).map(str::to_owned))
        });

    let _ = nmcli(&["connection", "delete", JOIN_CON_NAME]).await;

    // The group's beacon can take a moment to appear; retry the join.
    let mut last_err = None;
    for attempt in 1..=5u8 {
        let _ = nmcli(&["device", "wifi", "rescan"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        match nmcli(&[
            "device",
            "wifi",
            "connect",
            ssid,
            "password",
            password,
            "ifname",
            &ifname,
            "name",
            JOIN_CON_NAME,
            "hidden",
            "yes",
        ])
        .await
        {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                debug!("Join: attempt {attempt} to join '{ssid}' failed: {e}");
                let _ = nmcli(&["connection", "delete", JOIN_CON_NAME]).await;
                last_err = Some(e);
            }
        }
    }
    let guard = JoinGuard {
        ssid: ssid.to_string(),
        prior,
    };
    if let Some(e) = last_err {
        drop(guard); // restores the prior connection
        anyhow::bail!("couldn't join '{ssid}': {e}");
    }

    // Wait for an IPv4 address (DHCP from the phone).
    for _ in 0..25 {
        if let Ok(out) = nmcli(&["-g", "IP4.ADDRESS", "device", "show", &ifname]).await {
            if out
                .split('/')
                .next()
                .and_then(|s| s.trim().parse::<Ipv4Addr>().ok())
                .is_some()
            {
                info!("Join: on '{ssid}'");
                return Ok(guard);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    drop(guard);
    anyhow::bail!("joined '{ssid}' but got no address")
}

async fn nmcli(args: &[&str]) -> Result<String, anyhow::Error> {
    let out = Command::new("nmcli").args(args).output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "nmcli {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Brings up a WPA2 hotspot on the Wi-Fi interface and returns its live
/// credentials. Single-channel radios are taken off their current network for
/// the duration; NetworkManager reconnects them after teardown.
pub async fn start_hotspot() -> Result<HotspotGuard, anyhow::Error> {
    let devs = nmcli(&["-t", "-f", "DEVICE,TYPE", "device"]).await?;
    let ifname = devs
        .lines()
        .find_map(|l| l.strip_suffix(":wifi").map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("no Wi-Fi interface"))?;

    // Scoped: ThreadRng is !Send and must not live across an await.
    let (ssid, password) = {
        let mut rng = rand::rng();
        let suffix: String = (0..2)
            .map(|_| rng.sample(rand::distr::Alphanumeric) as char)
            .collect();
        let password: String = (0..12)
            .map(|_| rng.sample(rand::distr::Alphanumeric) as char)
            .collect();
        // P2P-style name; the phone treats it as the group to join.
        (format!("DIRECT-{suffix}-packet"), password)
    };

    // Drop any stale instance, then bring the hotspot up. 2.4GHz for reach and
    // because every client supports it.
    let _ = nmcli(&["connection", "delete", CON_NAME]).await;
    nmcli(&[
        "device", "wifi", "hotspot", "ifname", &ifname, "con-name", CON_NAME, "ssid", &ssid,
        "band", "bg", "password", &password,
    ])
    .await?;

    // Our address on the hotspot (the DHCP gateway) appears once activation
    // completes.
    let mut gateway = None;
    for _ in 0..20 {
        if let Ok(out) = nmcli(&["-g", "IP4.ADDRESS", "device", "show", &ifname]).await {
            if let Some(ip) = out
                .split('/')
                .next()
                .and_then(|s| s.trim().parse::<Ipv4Addr>().ok())
            {
                gateway = Some(ip);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let Some(gateway) = gateway else {
        let _ = nmcli(&["connection", "delete", CON_NAME]).await;
        anyhow::bail!("hotspot came up without an IPv4 gateway");
    };

    // Operating frequency (MHz) for the credentials; best-effort.
    let mut frequency = 2437;
    if let Ok(o) = Command::new("iw")
        .args(["dev", &ifname, "info"])
        .output()
        .await
    {
        let s = String::from_utf8_lossy(&o.stdout).into_owned();
        if let Some(pos) = s.find(" MHz") {
            let head = &s[..pos];
            if let Some(start) = head.rfind(|c: char| !c.is_ascii_digit()) {
                if let Ok(f) = head[start + 1..].parse::<i32>() {
                    frequency = f;
                }
            }
        }
    }

    info!(
        "Hotspot: '{ssid}' up on {ifname}, gateway {gateway}, freq {frequency} MHz. If the phone \
         joins but can't connect, allow the port once: sudo firewall-cmd --zone=nm-shared \
         --add-port={HOTSPOT_TCP_PORT}/tcp"
    );

    Ok(HotspotGuard {
        ssid,
        password,
        gateway,
        frequency,
    })
}
