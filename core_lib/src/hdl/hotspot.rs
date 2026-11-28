//! Hosting a Wi-Fi network for the send-side bandwidth upgrade (dynamic role
//! switch): when the phone and PC share no LAN, the phone asks the *sender* to
//! host the upgraded medium — exactly what Quick Share for Windows does. We
//! bring up a NetworkManager hotspot (`shared` mode gives us DHCP/NAT at the
//! gateway address), hand its credentials to the phone over the BLE channel,
//! and tear it down when the transfer ends.
use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::LazyLock;

use rand::RngExt;
use tokio::process::Command;
use tokio::sync::Mutex;

const CON_NAME: &str = "packet-qs-hotspot";
/// Serializes hotspot setup attempts so concurrent BWU retries cannot clobber
/// each other's `packet-qs-hotspot` NetworkManager profile.
static HOTSPOT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
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

struct FiveGhzCandidate {
    label: &'static str,
    channel: u16,
    channel_str: &'static str,
    default_freq: i32,
}

/// Non-DFS 5 GHz channels to try when the kernel/regulatory domain permits
/// standalone AP beaconing (no `no IR`, `disabled`, or `radar detection`).
const FIVE_GHZ_CANDIDATES: &[FiveGhzCandidate] = &[
    FiveGhzCandidate {
        label: "5GHz-ch149",
        channel: 149,
        channel_str: "149",
        default_freq: 5745,
    },
    FiveGhzCandidate {
        label: "5GHz-ch36",
        channel: 36,
        channel_str: "36",
        default_freq: 5180,
    },
];

/// Parses `iw list` output and returns the set of 5 GHz channel numbers that
/// allow standalone AP beaconing (i.e. not marked `disabled`, `no IR`, or
/// `radar detection`).
///
/// Pre-filtering avoids asking NetworkManager to activate a disallowed channel:
/// `wpa_supplicant` rejects `no IR` channels immediately, but NetworkManager
/// retries for ~25 seconds before failing, which blows past Android's BWU
/// negotiation window.
fn parse_ap_allowed_5ghz_channels(iw_list: &str) -> HashSet<u16> {
    let mut allowed = HashSet::new();
    for line in iw_list.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("* ") else {
            continue;
        };
        let Some((freq_str, after_mhz)) = rest.split_once(" MHz [") else {
            continue;
        };
        let Ok(freq_mhz) = freq_str.parse::<f32>() else {
            continue;
        };
        if !(5000.0..5900.0).contains(&freq_mhz) {
            continue;
        }
        let Some((ch_str, flags)) = after_mhz.split_once(']') else {
            continue;
        };
        let Ok(ch) = ch_str.parse::<u16>() else {
            continue;
        };
        let lower = flags.to_ascii_lowercase();
        if lower.contains("disabled")
            || lower.contains("no ir")
            || lower.contains("radar detection")
        {
            continue;
        }
        allowed.insert(ch);
    }
    allowed
}

async fn query_ap_allowed_5ghz_channels() -> HashSet<u16> {
    match Command::new("iw").arg("list").output().await {
        Ok(o) if o.status.success() => {
            parse_ap_allowed_5ghz_channels(&String::from_utf8_lossy(&o.stdout))
        }
        _ => HashSet::new(),
    }
}

/// Checks whether `ifname` is currently in AP mode via `iw dev <ifname> info`.
async fn confirm_ap_mode(ifname: &str) -> bool {
    match Command::new("iw")
        .args(["dev", ifname, "info"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains("type AP"),
        _ => false,
    }
}

/// Attempts to bring up a 5 GHz WPA2 AP on `candidate` (trying 80 MHz width
/// first, then auto-width if `wifi.channel-width` is rejected by an older
/// NetworkManager). Bounded with `--wait 4` so a driver failure never stalls
/// the BLE bandwidth-upgrade handshake.
async fn try_start_5ghz_ap(
    ifname: &str,
    ssid: &str,
    password: &str,
    candidate: &FiveGhzCandidate,
) -> Option<i32> {
    for width in [Some("80"), None] {
        let _ = nmcli(&["connection", "delete", CON_NAME]).await;
        let mut args = vec![
            "connection",
            "add",
            "type",
            "wifi",
            "ifname",
            ifname,
            "con-name",
            CON_NAME,
            "autoconnect",
            "no",
            "wifi.mode",
            "ap",
            "wifi.band",
            "a",
            "wifi.channel",
            candidate.channel_str,
            "wifi.ssid",
            ssid,
            "wifi.powersave",
            "2",
            "wifi-sec.key-mgmt",
            "wpa-psk",
            "wifi-sec.proto",
            "rsn",
            "wifi-sec.pairwise",
            "ccmp",
            "wifi-sec.psk",
            password,
            "ipv4.method",
            "shared",
            "ipv6.method",
            "ignore",
        ];
        if let Some(w) = width {
            args.extend(["wifi.channel-width", w]);
        }

        let tag = match width {
            Some(w) => format!("{}-{w}MHz", candidate.label),
            None => candidate.label.to_string(),
        };

        if let Err(e) = nmcli(&args).await {
            debug!("Hotspot: {tag} profile rejected: {e}");
            // Only fall through to the auto-width retry if `channel-width`
            // itself was rejected by NetworkManager.
            continue;
        }

        match nmcli(&["--wait", "4", "connection", "up", CON_NAME]).await {
            Ok(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                if confirm_ap_mode(ifname).await {
                    let _ = Command::new("iw")
                        .args(["dev", ifname, "set", "power_save", "off"])
                        .output()
                        .await;
                    debug!("Hotspot: activated profile {tag}");
                    return Some(candidate.default_freq);
                }
                debug!("Hotspot: {tag} reported up but {ifname} is not in AP mode");
                let _ = nmcli(&["connection", "delete", CON_NAME]).await;
                // If AP mode failed at the driver level, retrying the same
                // channel without channel-width won't help.
                return None;
            }
            Err(e) => {
                debug!("Hotspot: {tag} failed to activate: {e}");
                let _ = nmcli(&["connection", "delete", CON_NAME]).await;
                if width.is_none() {
                    return None;
                }
            }
        }
    }
    None
}

/// Brings up a WPA2 hotspot on the Wi-Fi interface and returns its live
/// credentials. Prefers 5 GHz (80 MHz on non-DFS channels when permitted by the
/// adapter's current regulatory domain) and falls back immediately to 2.4 GHz
/// (`band bg`) otherwise. Single-channel radios are taken off their current
/// network for the duration; NetworkManager reconnects them after teardown.
pub async fn start_hotspot() -> Result<HotspotGuard, anyhow::Error> {
    let _guard = HOTSPOT_LOCK.lock().await;

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

    // Only attempt 5 GHz on channels the kernel currently allows for AP mode
    // (no `no IR` / `disabled` / `radar detection`), so cards in world regdom
    // `00` (e.g. Intel LAR) fall straight through to 2.4 GHz in milliseconds.
    let allowed_5ghz = query_ap_allowed_5ghz_channels().await;
    let mut frequency = None;
    for candidate in FIVE_GHZ_CANDIDATES {
        if !allowed_5ghz.contains(&candidate.channel) {
            debug!(
                "Hotspot: skipping {} (ch {} restricted or unavailable in iw list)",
                candidate.label, candidate.channel
            );
            continue;
        }
        if let Some(freq) = try_start_5ghz_ap(&ifname, &ssid, &password, candidate).await {
            frequency = Some(freq);
            break;
        }
    }

    let mut frequency = match frequency {
        Some(f) => f,
        None => {
            // Universal 2.4 GHz fallback (matches NetworkManager's default
            // hotspot helper so every adapter and regulatory domain works).
            let _ = nmcli(&["connection", "delete", CON_NAME]).await;
            nmcli(&[
                "device", "wifi", "hotspot", "ifname", &ifname, "con-name", CON_NAME, "ssid",
                &ssid, "band", "bg", "password", &password,
            ])
            .await?;
            2437
        }
    };

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

#[cfg(test)]
mod tests {
    use super::parse_ap_allowed_5ghz_channels;

    #[test]
    fn skips_no_ir_dfs_and_disabled_5ghz_channels() {
        let sample = "\
\t\t\t* 2412.0 MHz [1] (22.0 dBm)
\t\t\t* 5180.0 MHz [36] (22.0 dBm) (no IR)
\t\t\t* 5260.0 MHz [52] (22.0 dBm) (no IR, radar detection)
\t\t\t* 5340.0 MHz [68] (disabled)
\t\t\t* 5745.0 MHz [149] (22.0 dBm) (no IR)
";
        assert!(parse_ap_allowed_5ghz_channels(sample).is_empty());
    }

    #[test]
    fn collects_clean_non_dfs_5ghz_channels() {
        let sample = "\
\t\t\t* 2437.0 MHz [6] (22.0 dBm)
\t\t\t* 5180.0 MHz [36] (23.0 dBm) (no outdoor)
\t\t\t* 5260.0 MHz [52] (20.0 dBm) (radar detection)
\t\t\t* 5745.0 MHz [149] (30.0 dBm)
\t\t\t* 5955.0 MHz [1] (23.0 dBm)
";
        let allowed = parse_ap_allowed_5ghz_channels(sample);
        assert!(allowed.contains(&36));
        assert!(allowed.contains(&149));
        assert!(!allowed.contains(&52));
        assert!(!allowed.contains(&6));
        assert!(!allowed.contains(&1));
    }
}
