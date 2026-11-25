//! A name resolver that still works when the system one refuses.
//!
//! On some Chinese ISPs the system resolver answers `api.cloudflare.com` with
//! `EBADRESP`/`os error 11001` while the very same name resolves instantly over
//! plain UDP to any public server. Without this fallback the edge purge fails
//! silently and uploads stall with "不知道这样的主机".
//!
//! The system resolver is still tried first: it is the only one that knows about
//! hosts files, VPN split DNS and corporate suffixes. Only when it fails do we
//! ask the public servers directly.

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

/// Asked in this order; all of them answered A queries in the affected network.
pub const SERVERS: [&str; 4] = ["223.5.5.5", "119.29.29.29", "114.114.114.114", "8.8.8.8"];
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Default)]
pub struct FallbackResolver;

impl Resolver for FallbackResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if let Ok(addrs) = DefaultResolver::default().resolve(uri, config, timeout) {
            if !addrs.is_empty() {
                return Ok(addrs);
            }
        }

        let Some(host) = uri.host() else {
            return Err(ureq::Error::HostNotFound);
        };
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });

        let ips = query_a(host).ok_or(ureq::Error::HostNotFound)?;
        let mut out = self.empty();
        for ip in ips {
            out.push(SocketAddr::new(ip.into(), port));
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        log::debug!("resolved {host} via fallback DNS: {:?}", out);
        Ok(out)
    }
}

/// Resolve a name by pasting a DNS question onto the wire and reading A records
/// out of the answer. Deliberately IPv4-only: R2 and Cloudflare both are.
pub fn query_a(host: &str) -> Option<Vec<Ipv4Addr>> {
    let host = host.trim();
    if host.is_empty() || host.parse::<Ipv4Addr>().is_ok() {
        return host.parse::<Ipv4Addr>().ok().map(|ip| vec![ip]);
    }
    let packet = build_query(host, 0x6e64)?;
    for server in SERVERS {
        if let Some(ips) = ask(server, &packet) {
            if !ips.is_empty() {
                return Some(ips);
            }
        }
    }
    None
}

fn ask(server: &str, packet: &[u8]) -> Option<Vec<Ipv4Addr>> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(QUERY_TIMEOUT)).ok()?;
    socket.connect((server, 53)).ok()?;
    socket.send(packet).ok()?;
    let mut buf = [0u8; 512];
    let read = socket.recv(&mut buf).ok()?;
    parse_a(&buf[..read])
}

fn build_query(host: &str, id: u16) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00]); // standard query, recursion desired
    out.extend_from_slice(&[0x00, 0x01]); // one question
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&[0x00, 0x01]); // type A
    out.extend_from_slice(&[0x00, 0x01]); // class IN
    Some(out)
}

fn parse_a(buf: &[u8]) -> Option<Vec<Ipv4Addr>> {
    if buf.len() < 12 {
        return None;
    }
    let questions = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let answers = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut pos = 12;
    for _ in 0..questions {
        pos = skip_name(buf, pos)?;
        pos = pos.checked_add(4)?; // type + class
    }
    let mut out = Vec::new();
    for _ in 0..answers {
        pos = skip_name(buf, pos)?;
        if buf.len() < pos + 10 {
            return None;
        }
        let record_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let length = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if buf.len() < pos + length {
            return None;
        }
        if record_type == 1 && length == 4 {
            out.push(Ipv4Addr::new(
                buf[pos],
                buf[pos + 1],
                buf[pos + 2],
                buf[pos + 3],
            ));
        }
        pos += length;
    }
    Some(out)
}

/// Walk a possibly compressed name and return the offset just past it.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let length = *buf.get(pos)?;
        if length == 0 {
            return Some(pos + 1);
        }
        if length & 0xc0 == 0xc0 {
            return Some(pos + 2);
        }
        pos = pos.checked_add(1 + length as usize)?;
        if pos > buf.len() {
            return None;
        }
    }
}

/// Used by `--resolve` so a broken resolver is diagnosable without a packet capture.
pub fn system_lookup(host: &str) -> Result<Vec<SocketAddr>, String> {
    match (host, 443u16).to_socket_addrs() {
        Ok(iter) => Ok(iter.collect()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_has_the_expected_header_and_labels() {
        let packet = build_query("acc.r2.cloudflarestorage.com", 0x6e64).unwrap();
        assert_eq!(&packet[0..2], &[0x6e, 0x64]);
        assert_eq!(&packet[2..4], &[0x01, 0x00]);
        assert_eq!(&packet[4..6], &[0x00, 0x01]);
        assert_eq!(packet[12], 3);
        assert_eq!(&packet[13..16], b"acc");
        // The packet must end with the root label plus type A and class IN.
        assert_eq!(&packet[packet.len() - 5..], &[0x00, 0x00, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn an_ip_address_is_its_own_answer() {
        assert_eq!(
            query_a("104.19.192.175"),
            Some(vec![Ipv4Addr::new(104, 19, 192, 175)])
        );
        assert_eq!(query_a(""), None);
    }

    #[test]
    fn answers_are_read_past_a_compressed_name() {
        let mut buf = vec![
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
        ];
        // question: example.com A IN
        buf.extend_from_slice(&[7]);
        buf.extend_from_slice(b"example");
        buf.extend_from_slice(&[3]);
        buf.extend_from_slice(b"com");
        buf.push(0);
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
        // two answers, both pointing back at the question name
        for octets in [[1u8, 2, 3, 4], [5, 6, 7, 8]] {
            buf.extend_from_slice(&[0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
            buf.extend_from_slice(&[0x00, 0x3c, 0x00, 0x04]);
            buf.extend_from_slice(&octets);
        }
        assert_eq!(
            parse_a(&buf),
            Some(vec![Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8)])
        );
    }

    #[test]
    fn a_cname_answer_is_skipped_not_mistaken_for_an_address() {
        let mut buf = vec![
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        buf.extend_from_slice(&[1]);
        buf.extend_from_slice(b"a");
        buf.extend_from_slice(&[3]);
        buf.extend_from_slice(b"com");
        buf.push(0);
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
        // CNAME with 4 bytes of target data
        buf.extend_from_slice(&[0xc0, 0x0c, 0x00, 0x05, 0x00, 0x01, 0x00, 0x00]);
        buf.extend_from_slice(&[0x00, 0x3c, 0x00, 0x04, 0xc0, 0x0c, 0x00, 0x00]);
        assert_eq!(parse_a(&buf), Some(Vec::new()));
    }

    #[test]
    fn a_truncated_packet_is_not_trusted() {
        assert_eq!(parse_a(&[0x00, 0x01]), None);
        assert_eq!(parse_a(&[0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]), None);
    }
}
