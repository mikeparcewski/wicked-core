//! (core#1377 follow-up) Seat networks get a small subnet from a wicked-owned pool.
//!
//! Each seat network once took one of the runtime's default address pools: Docker's defaults give
//! about 31 networks (15 × /16 + 16 × /20), so a host ran out at about three concurrent runs. A
//! seat network holds the seat container, its network sidecar and the gateway's endpoint, so a
//! /26 (62 hosts) is ample. The engine now creates each seat network with an explicit subnet from
//! [`POOL_ENV`] (default [`DEFAULT_POOL`], 1024 × /26), chosen against every subnet the runtime's
//! networks already hold and every route on the host (a VPN or corporate route is never
//! shadowed). Two daemons that pick the same free slot at once are told so by the runtime ("Pool
//! overlaps"); the loser moves to its next slot ([`is_overlap_refusal`]). A slot is released when
//! its network is removed (the reap, or the sweep). `WICKED_SEAT_SUBNET_POOL=runtime` keeps the
//! runtime's own pools.

use std::net::Ipv4Addr;

/// The pool seat subnets are cut from (an IPv4 CIDR), or `runtime` for the runtime's own pools.
pub(crate) const POOL_ENV: &str = "WICKED_SEAT_SUBNET_POOL";
/// The prefix length of one seat subnet (24..=28).
pub(crate) const PREFIX_ENV: &str = "WICKED_SEAT_SUBNET_PREFIX";
pub(crate) const DEFAULT_POOL: &str = "10.213.0.0/16";
pub(crate) const DEFAULT_PREFIX: u8 = 26;
/// How many free slots one network create tries before it gives up.
pub(crate) const MAX_TRIES: usize = 8;

/// An IPv4 network: its first address and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cidr {
    pub(crate) base: u32,
    pub(crate) len: u8,
}

fn mask(len: u8) -> u32 {
    if len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(len))
    }
}

impl Cidr {
    /// `a.b.c.d/n`, normalised to its network address. `None` for anything else (IPv6 included).
    pub(crate) fn parse(s: &str) -> Option<Cidr> {
        let (addr, len) = s.trim().split_once('/')?;
        let ip: Ipv4Addr = addr.parse().ok()?;
        let len: u8 = len.parse().ok()?;
        (len <= 32).then(|| Cidr {
            base: u32::from(ip) & mask(len),
            len,
        })
    }

    pub(crate) fn last(&self) -> u32 {
        self.base | !mask(self.len)
    }

    pub(crate) fn overlaps(&self, other: &Cidr) -> bool {
        self.base <= other.last() && other.base <= self.last()
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.base), self.len)
    }
}

/// How seat networks get their subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// The runtime's own default pools (`WICKED_SEAT_SUBNET_POOL=runtime`).
    Runtime,
    /// `prefix`-sized slots cut from `pool`.
    Pool { pool: Cidr, prefix: u8 },
}

/// The plan for the given env values (`None` = unset). A pool that is not an IPv4 CIDR (or
/// `runtime`), or a prefix outside 24..=28 or wider than the pool, is refused with why; a value
/// that is set but empty is refused too (never a silent fallback to the default).
pub(crate) fn plan_from(pool: Option<&str>, prefix: Option<&str>) -> Result<Plan, String> {
    let pool = pool.map(str::trim);
    let prefix = match prefix.map(str::trim) {
        None => DEFAULT_PREFIX,
        Some(p) => p
            .parse::<u8>()
            .ok()
            .filter(|n| (24..=28).contains(n))
            .ok_or_else(|| format!("{PREFIX_ENV}=`{p}` is not a prefix length in 24..=28"))?,
    };
    if pool == Some("runtime") {
        return Ok(Plan::Runtime);
    }
    let raw = pool.unwrap_or(DEFAULT_POOL);
    let pool = Cidr::parse(raw)
        .ok_or_else(|| format!("{POOL_ENV}=`{raw}` is not an IPv4 CIDR (or `runtime`)"))?;
    // At most a /12: the slots a search walks stay bounded (65,536 at /28).
    if pool.len < 12 {
        return Err(format!("{POOL_ENV}={pool} is wider than a /12"));
    }
    if prefix < pool.len {
        return Err(format!(
            "{PREFIX_ENV}={prefix} is wider than the pool {pool} it is cut from"
        ));
    }
    Ok(Plan::Pool { pool, prefix })
}

/// The plan this process's environment asks for (a value that is not Unicode is refused).
pub(crate) fn plan() -> Result<Plan, String> {
    let read = |k: &str| match std::env::var(k) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{k} is not valid Unicode")),
    };
    plan_from(read(POOL_ENV)?.as_deref(), read(PREFIX_ENV)?.as_deref())
}

/// One BSD `netstat -rn` destination (macOS): `default` is no network; an abbreviated one
/// (`127`, `169.254`, `192.168.68/22`) is padded with zero octets, its length the octets given
/// times 8 when it names none; a full address with no length is a host route (/32).
pub(crate) fn parse_bsd_destination(d: &str) -> Option<Cidr> {
    let (addr, len) = match d.split_once('/') {
        Some((a, l)) => (a, Some(l.parse::<u8>().ok()?)),
        None => (d, None),
    };
    let octets: Vec<u8> = addr
        .split('.')
        .map(|o| o.parse::<u8>().ok())
        .collect::<Option<_>>()?;
    if octets.is_empty() || octets.len() > 4 {
        return None;
    }
    let len = len.unwrap_or(8 * octets.len() as u8);
    let mut full = [0u8; 4];
    full[..octets.len()].copy_from_slice(&octets);
    Cidr::parse(&format!("{}/{len}", Ipv4Addr::from(full)))
}

/// The IPv4 routes in `netstat -rn -f inet` output (macOS), the default route left out.
pub(crate) fn routes_from_netstat(out: &str) -> Vec<Cidr> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(parse_bsd_destination)
        .filter(|c| c.len > 0)
        .collect()
}

/// The routes in `ip -4 route show table all` output (every table, policy routing included): the
/// destination is the first word, or the second after a route type (`local`, `broadcast`, ...); a
/// bare address is a host route; `default` is left out.
pub(crate) fn routes_from_ip(out: &str) -> Vec<Cidr> {
    const TYPES: &[&str] = &[
        "unicast",
        "local",
        "broadcast",
        "multicast",
        "throw",
        "unreachable",
        "prohibit",
        "blackhole",
        "nat",
        "anycast",
    ];
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let first = w.next()?;
            let dest = if TYPES.contains(&first) {
                w.next()?
            } else {
                first
            };
            if dest == "default" {
                return None;
            }
            if dest.contains('/') {
                Cidr::parse(dest)
            } else {
                Cidr::parse(&format!("{dest}/32"))
            }
        })
        .filter(|c| c.len > 0)
        .collect()
}

/// This host's IPv4 routes. `Err` when they cannot be read: the allocation is then refused, never
/// made blind to a VPN or corporate route.
pub(crate) fn host_routes() -> Result<Vec<Cidr>, String> {
    let run = |bin: &str, args: &[&str]| {
        use wicked_apps_core::spawn::HardenedCommand;
        let mut c = std::process::Command::new(bin);
        c.hardened().args(args);
        crate::container_floor::output_within(c, std::time::Duration::from_secs(10))
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if cfg!(target_os = "linux") {
        let found = crate::validator::find_on_path("ip");
        let fixed =
            ["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip", "/bin/ip"].map(std::path::PathBuf::from);
        for ip in found.iter().chain(fixed.iter()).filter(|p| p.exists()) {
            if let Some(out) = run(
                &ip.to_string_lossy(),
                &["-4", "route", "show", "table", "all"],
            ) {
                return Ok(routes_from_ip(&out));
            }
        }
        // `/proc/net/route` holds the main table only: a VPN route in another table would be
        // missed, so it is never taken as the whole answer.
        return Err(format!(
            "the host's routes could not be read in every table (`ip -4 route show table all`); \
             put iproute2's `ip` on the PATH, or set {POOL_ENV}=runtime for the runtime's own \
             pools"
        ));
    }
    if cfg!(target_os = "macos") {
        return run("/usr/sbin/netstat", &["-rn", "-f", "inet"])
            .map(|o| routes_from_netstat(&o))
            .ok_or_else(|| "the host's routes could not be read (`netstat -rn -f inet`)".into());
    }
    Err("this host's routes cannot be read to choose a seat subnet".into())
}

/// The networks in `network inspect` JSON (an array) by name, each with its IPv4 subnets: docker
/// and nerdctl spell them `IPAM.Config[].Subnet`, podman `subnets[].subnet`. IPv6 is skipped.
/// `None` when the answer is not such an array.
pub(crate) fn networks_in(json: &str) -> Option<Vec<(String, Vec<Cidr>)>> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let nets = v.as_array()?;
    Some(
        nets.iter()
            .filter_map(|n| {
                let name = n
                    .get("Name")
                    .or_else(|| n.get("name"))?
                    .as_str()?
                    .to_string();
                let docker = n
                    .pointer("/IPAM/Config")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("Subnet")?.as_str());
                let podman = n
                    .get("subnets")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("subnet")?.as_str());
                Some((name, docker.chain(podman).filter_map(Cidr::parse).collect()))
            })
            .collect(),
    )
}

/// The free slots of `pool` (`prefix`-sized) that overlap nothing `taken`, at most `max`, in the
/// order a network named with `seed` tries them: from a slot the seed picks, wrapping around, so
/// two engines creating at once rarely start on the same one.
pub(crate) fn candidates(
    pool: Cidr,
    prefix: u8,
    taken: &[Cidr],
    seed: u64,
    max: usize,
) -> Vec<Cidr> {
    let slots: u64 = 1u64 << (prefix - pool.len);
    let start = seed % slots;
    (0..slots)
        .map(|i| (start + i) % slots)
        .map(|k| Cidr {
            base: pool.base + ((k as u32) << (32 - u32::from(prefix))),
            len: prefix,
        })
        .filter(|c| !taken.iter().any(|t| t.overlaps(c)))
        .take(max)
        .collect()
}

/// A stable seed for a network name (FNV-1a).
pub(crate) fn seed_of(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Whether a network create was refused because its subnet is already used (another engine took
/// the slot first, or a network the listing missed): Docker's "Pool overlaps with other one on
/// this address space", podman's "subnet ... is already used".
pub(crate) fn is_overlap_refusal(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("pool overlaps") || e.contains("overlaps with") || e.contains("already used")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Cidr {
        Cidr::parse(s).unwrap()
    }

    #[test]
    fn cidrs_parse_normalise_and_overlap() {
        assert_eq!(c("10.213.5.7/16").to_string(), "10.213.0.0/16");
        assert!(Cidr::parse("fc00::/64").is_none() && Cidr::parse("10.0.0.0/33").is_none());
        assert!(c("10.213.0.0/16").overlaps(&c("10.213.4.0/26")));
        assert!(c("10.0.0.0/8").overlaps(&c("10.213.0.0/16")));
        assert!(!c("10.213.0.0/26").overlaps(&c("10.213.0.64/26")));
    }

    #[test]
    fn the_plan_reads_its_env_and_refuses_what_it_cannot_use() {
        assert_eq!(
            plan_from(None, None),
            Ok(Plan::Pool {
                pool: c(DEFAULT_POOL),
                prefix: DEFAULT_PREFIX
            })
        );
        assert_eq!(plan_from(Some("runtime"), None), Ok(Plan::Runtime));
        assert!(plan_from(Some("runtime"), Some("x")).is_err());
        assert!(plan_from(Some("10.0.0.0/8"), None).is_err());
        assert_eq!(
            plan_from(Some("172.30.0.0/20"), Some("28")),
            Ok(Plan::Pool {
                pool: c("172.30.0.0/20"),
                prefix: 28
            })
        );
        assert!(plan_from(Some("nope"), None).is_err());
        assert!(plan_from(Some(""), None).is_err() && plan_from(None, Some(" ")).is_err());
        assert!(plan_from(None, Some("16")).is_err());
        assert!(plan_from(Some("10.0.0.0/27"), Some("26")).is_err());
    }

    #[test]
    fn macos_routes_are_read_with_abbreviated_destinations() {
        let out =
            "Routing tables\n\nInternet:\nDestination        Gateway            Flags  Netif\n\
                   default            192.168.68.1       UGScg  en0\n\
                   10.213.64/18       utun4              USc    utun4\n\
                   127                127.0.0.1          UCS    lo0\n\
                   169.254            link#14            UCS    en0\n\
                   192.168.68/22      link#14            UCS    en0\n\
                   192.168.68.54      a:b:c:d:e:f        UHLWI  en0\n\
                   224.0.0/4          link#14            UmCS   en0\n";
        let r = routes_from_netstat(out);
        let shown: Vec<String> = r.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "10.213.64.0/18",
                "127.0.0.0/8",
                "169.254.0.0/16",
                "192.168.68.0/22",
                "192.168.68.54/32",
                "224.0.0.0/4"
            ]
        );
    }

    #[test]
    fn ip_routes_are_read_from_every_table() {
        let out = "default via 192.168.1.1 dev eth0\n\
                   10.213.0.0/16 dev tun0 table 100 scope link\n\
                   local 192.168.1.5 dev eth0 table local proto kernel scope host src 192.168.1.5\n\
                   broadcast 192.168.1.255 dev eth0 table local proto kernel scope link\n\
                   172.18.0.0/16 dev br-x proto kernel scope link src 172.18.0.1 linkdown\n";
        let shown: Vec<String> = routes_from_ip(out)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                "10.213.0.0/16",
                "192.168.1.5/32",
                "192.168.1.255/32",
                "172.18.0.0/16"
            ]
        );
    }

    #[test]
    fn candidates_skip_taken_slots_and_routes_and_start_where_the_seed_says() {
        let pool = c("10.213.0.0/24");
        // 4 × /26: .0, .64, .128, .192.
        let taken = [c("10.213.0.64/26"), c("10.213.0.200/32")];
        let got: Vec<String> = candidates(pool, 26, &taken, 1, 8)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, ["10.213.0.128/26", "10.213.0.0/26"]);
        // A route covering the whole pool leaves nothing.
        assert!(candidates(pool, 26, &[c("10.0.0.0/8")], 0, 8).is_empty());
        assert_eq!(
            candidates(c(DEFAULT_POOL), 26, &[], 0, MAX_TRIES).len(),
            MAX_TRIES
        );
        assert_ne!(seed_of("wicked-seat-a"), seed_of("wicked-seat-b"));
    }

    #[test]
    fn runtime_answers_are_read() {
        let docker = r#"[{"Name":"bridge","IPAM":{"Config":[{"Subnet":"172.17.0.0/16"}]}},
            {"Name":"kind","IPAM":{"Config":[{"Subnet":"fc00:f853:ccd:e793::/64"},{"Subnet":"192.168.16.0/20"}]}},
            {"Name":"none","IPAM":{"Config":[]}}]"#;
        assert_eq!(
            networks_in(docker).unwrap(),
            [
                ("bridge".to_string(), vec![c("172.17.0.0/16")]),
                ("kind".to_string(), vec![c("192.168.16.0/20")]),
                ("none".to_string(), vec![])
            ]
        );
        let podman =
            r#"[{"name":"podman","subnets":[{"subnet":"10.88.0.0/16","gateway":"10.88.0.1"}]}]"#;
        assert_eq!(
            networks_in(podman).unwrap(),
            [("podman".to_string(), vec![c("10.88.0.0/16")])]
        );
        assert!(networks_in("{{.Subnet}}").is_none());
        assert!(is_overlap_refusal(
            "Error response from daemon: Pool overlaps with other one on this address space"
        ));
        assert!(is_overlap_refusal(
            "Error: subnet 10.213.0.0/26 is already used on the host"
        ));
        assert!(!is_overlap_refusal(
            "all predefined address pools have been fully subnetted"
        ));
    }
}
