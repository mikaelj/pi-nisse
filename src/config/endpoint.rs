//! Endpoint trust validation.
//!
//! By default, only endpoints whose host is (or resolves to) a permitted
//! local address class are trusted: IPv4/IPv6 loopback, RFC1918 private
//! IPv4 (`10/8`, `172.16/12`, `192.168/16`), and IPv6 unique-local
//! (`fc00::/7`). Hostnames are always resolved before the trust decision;
//! a name is never trusted by spelling alone. Permitting non-private
//! endpoints requires the explicit trusted-local-configuration override
//! `network.allow_non_private_endpoints = true`.
//!
//! Trust validation never performs model requests; it may perform hostname
//! resolution only.

use std::io;
use std::net::{IpAddr, Ipv6Addr, ToSocketAddrs};

use thiserror::Error;
use url::{Host, Url};

/// Classification of an IP address for endpoint trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressClass {
    /// IPv4 `127.0.0.0/8` or IPv6 `::1`.
    Loopback,
    /// RFC1918 private IPv4: `10/8`, `172.16/12`, `192.168/16`.
    PrivateV4,
    /// IPv6 unique-local `fc00::/7`.
    UniqueLocalV6,
    /// Everything else: not trusted unless explicitly overridden.
    Public,
}

impl AddressClass {
    /// Short human-readable label used by `nisse doctor`.
    pub fn label(self) -> &'static str {
        match self {
            AddressClass::Loopback => "loopback",
            AddressClass::PrivateV4 => "private (RFC1918)",
            AddressClass::UniqueLocalV6 => "unique-local (fc00::/7)",
            AddressClass::Public => "public",
        }
    }
}

/// Whether `class` belongs to the default-permitted endpoint classes.
pub fn is_trusted_class(class: AddressClass) -> bool {
    matches!(
        class,
        AddressClass::Loopback | AddressClass::PrivateV4 | AddressClass::UniqueLocalV6
    )
}

/// Classify `ip` into the endpoint trust classes.
///
/// IPv4-mapped IPv6 addresses classify as their embedded IPv4 address.
pub fn classify_ip(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                AddressClass::Loopback
            } else if v4.is_private() {
                AddressClass::PrivateV4
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_ip(IpAddr::V4(v4));
            }
            if v6.is_loopback() {
                AddressClass::Loopback
            } else if is_unique_local_v6(&v6) {
                AddressClass::UniqueLocalV6
            } else {
                AddressClass::Public
            }
        }
    }
}

fn is_unique_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// Hostname resolution abstraction so trust decisions can be tested without
/// network access.
pub trait HostResolver: Send + Sync {
    /// Resolve `host` to its IP addresses.
    fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// Resolver backed by the platform resolver.
#[derive(Debug, Default)]
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        // The port is ignored by name resolution; 0 implies no service.
        Ok((host, 0u16)
            .to_socket_addrs()?
            .map(|addr| addr.ip())
            .collect())
    }
}

/// Failures raised while deciding whether an endpoint may be used.
#[derive(Debug, Error)]
pub enum EndpointTrustError {
    /// The endpoint is not a usable HTTP(S) URL.
    #[error("`{url}` is not a usable endpoint URL: {reason}")]
    InvalidUrl { url: String, reason: String },
    /// The hostname could not be resolved.
    #[error("could not resolve `{host}` (`{url}`): {source}")]
    Resolution {
        url: String,
        host: String,
        #[source]
        source: io::Error,
    },
    /// The hostname resolved to no addresses.
    #[error("`{host}` (`{url}`) resolved to no addresses")]
    NoAddresses { url: String, host: String },
    /// An address outside the permitted classes was found.
    #[error(
        "`{url}` uses a non-permitted address {address}; default-permitted classes are \
         loopback, RFC1918 private, and IPv6 unique-local (trusted local configuration \
         may set `network.allow_non_private_endpoints = true`)"
    )]
    UntrustedAddress {
        url: String,
        host: String,
        address: IpAddr,
    },
}

/// How an endpoint's host was interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointHost {
    /// The host was an IP literal.
    Ip(IpAddr),
    /// The host was a name that requires resolution.
    Name(String),
}

/// A syntactically parsed endpoint URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEndpoint {
    /// Parsed URL; the scheme is guaranteed to be `http` or `https`.
    pub url: Url,
    /// Display form of the host.
    pub host: String,
    /// Interpreted host kind.
    pub host_kind: EndpointHost,
}

/// Parse an endpoint URL and enforce HTTP(S) scheme and host presence.
///
/// This performs no trust decision; it only rejects unusable URLs.
pub fn parse_endpoint_url(endpoint: &str) -> Result<ParsedEndpoint, EndpointTrustError> {
    let invalid = |reason: String| EndpointTrustError::InvalidUrl {
        url: endpoint.to_string(),
        reason,
    };

    let url = Url::parse(endpoint).map_err(|err| invalid(format!("invalid URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "scheme `{}` is not `http` or `https`",
            url.scheme()
        )));
    }
    let host_kind = match url.host() {
        Some(Host::Ipv4(v4)) => EndpointHost::Ip(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => EndpointHost::Ip(IpAddr::V6(v6)),
        Some(Host::Domain(domain)) if !domain.is_empty() => EndpointHost::Name(domain.to_string()),
        _ => return Err(invalid("URL has no host".to_string())),
    };
    let host = url.host_str().unwrap_or_default().to_string();
    Ok(ParsedEndpoint {
        url,
        host,
        host_kind,
    })
}

/// A trusted endpoint together with the reason it was trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedEndpoint {
    /// The endpoint URL as configured.
    pub url: String,
    /// The endpoint host.
    pub host: String,
    /// Why the endpoint is permitted.
    pub detail: String,
}

/// Decide whether `endpoint` may be used under the given network policy.
///
/// Order of decisions:
///
/// 1. the URL must parse as HTTP(S) with a host;
/// 2. if non-private endpoints are explicitly permitted, the endpoint is
///    allowed without resolution;
/// 3. an IP literal is classified directly;
/// 4. a hostname is resolved and *every* resolved address must belong to a
///    permitted class.
pub fn check_endpoint_trust(
    endpoint: &str,
    allow_non_private_endpoints: bool,
    resolver: &dyn HostResolver,
) -> Result<TrustedEndpoint, EndpointTrustError> {
    let ParsedEndpoint {
        url: _,
        host,
        host_kind,
    } = parse_endpoint_url(endpoint)?;

    if allow_non_private_endpoints {
        return Ok(TrustedEndpoint {
            url: endpoint.to_string(),
            host,
            detail: "permitted by trusted local configuration override".to_string(),
        });
    }

    match host_kind {
        EndpointHost::Ip(address) => {
            let class = classify_ip(address);
            if is_trusted_class(class) {
                Ok(TrustedEndpoint {
                    url: endpoint.to_string(),
                    host,
                    detail: format!("{} address literal", class.label()),
                })
            } else {
                Err(EndpointTrustError::UntrustedAddress {
                    url: endpoint.to_string(),
                    host,
                    address,
                })
            }
        }
        EndpointHost::Name(name) => {
            let addresses =
                resolver
                    .resolve(&name)
                    .map_err(|source| EndpointTrustError::Resolution {
                        url: endpoint.to_string(),
                        host: name.clone(),
                        source,
                    })?;
            if addresses.is_empty() {
                return Err(EndpointTrustError::NoAddresses {
                    url: endpoint.to_string(),
                    host: name,
                });
            }
            for &address in &addresses {
                if !is_trusted_class(classify_ip(address)) {
                    return Err(EndpointTrustError::UntrustedAddress {
                        url: endpoint.to_string(),
                        host: name,
                        address,
                    });
                }
            }
            let joined = addresses
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(TrustedEndpoint {
                url: endpoint.to_string(),
                host: name,
                detail: format!("resolved to permitted addresses: {joined}"),
            })
        }
    }
}

/// Deterministic fake resolver for tests.
#[cfg(test)]
#[derive(Default)]
pub struct FakeResolver {
    /// Addresses returned for every hostname.
    pub ips: Vec<IpAddr>,
    /// When set, resolution fails.
    pub fail: bool,
}

#[cfg(test)]
impl HostResolver for FakeResolver {
    fn resolve(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
        if self.fail {
            Err(io::Error::other("resolution unavailable"))
        } else {
            Ok(self.ips.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn v4(octets: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(octets))
    }

    fn v6(segments: [u16; 8]) -> IpAddr {
        IpAddr::V6(std::net::Ipv6Addr::from(segments))
    }

    #[test]
    fn classifies_ipv4_loopback() {
        for ip in ["127.0.0.1", "127.255.0.1", "127.0.0.0"] {
            assert_eq!(
                classify_ip(v4(ip.parse::<Ipv4Addr>().unwrap().octets())),
                AddressClass::Loopback,
                "{ip}"
            );
        }
    }

    #[test]
    fn classifies_rfc1918_private_ipv4() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.20.7.7",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.77.4",
        ] {
            assert_eq!(
                classify_ip(v4(ip.parse::<Ipv4Addr>().unwrap().octets())),
                AddressClass::PrivateV4,
                "{ip}"
            );
        }
    }

    #[test]
    fn rejects_non_rfc1918_ipv4_as_public() {
        for ip in [
            "8.8.8.8",
            "172.15.255.255", // just below 172.16/12
            "172.32.0.1",     // just above 172.16/12
            "11.0.0.1",
            "169.254.1.1", // link-local is not a permitted class
            "0.0.0.0",
            "100.64.0.1", // CGNAT is not RFC1918
        ] {
            assert_eq!(
                classify_ip(v4(ip.parse::<Ipv4Addr>().unwrap().octets())),
                AddressClass::Public,
                "{ip}"
            );
        }
    }

    #[test]
    fn classifies_ipv6() {
        assert_eq!(
            classify_ip(v6([0, 0, 0, 0, 0, 0, 0, 1])),
            AddressClass::Loopback
        );
        assert_eq!(
            classify_ip(v6([0xfc00, 1, 0, 0, 0, 0, 0, 1])),
            AddressClass::UniqueLocalV6
        );
        assert_eq!(
            classify_ip(v6([0xfd12, 0, 0, 0, 0, 0, 0, 7])),
            AddressClass::UniqueLocalV6
        );
        assert_eq!(
            classify_ip(v6([0xfe80, 0, 0, 0, 0, 0, 0, 1])),
            AddressClass::Public
        );
        assert_eq!(
            classify_ip(v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])),
            AddressClass::Public
        );
    }

    #[test]
    fn classifies_ipv4_mapped_ipv6_as_embedded_ipv4() {
        // ::ffff:127.0.0.1
        let mapped_loopback: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(classify_ip(mapped_loopback), AddressClass::Loopback);
        // ::ffff:8.8.8.8
        let mapped_public: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert_eq!(classify_ip(mapped_public), AddressClass::Public);
    }

    #[test]
    fn parse_rejects_unusable_urls() {
        for endpoint in [
            "not a url",
            "ftp://127.0.0.1/files",
            "http://",        // empty host
            "http://[::1",    // malformed IPv6 literal
            "localhost:8080", // not an HTTP(S) URL
            "",
        ] {
            assert!(
                parse_endpoint_url(endpoint).is_err(),
                "expected `{endpoint}` to be rejected"
            );
        }
    }

    #[test]
    fn parse_accepts_http_and_https_with_hosts() {
        for endpoint in [
            "http://127.0.0.1:8080/v1",
            "https://modelbox.internal:443/",
            "http://[::1]:9000/v1",
            "https://10.0.0.5",
        ] {
            let parsed = parse_endpoint_url(endpoint)
                .unwrap_or_else(|err| panic!("expected `{endpoint}` to parse: {err}"));
            assert!(matches!(parsed.url.scheme(), "http" | "https"));
        }
        assert!(matches!(
            parse_endpoint_url("http://[::1]:9000/v1")
                .unwrap()
                .host_kind,
            EndpointHost::Ip(IpAddr::V6(_))
        ));
        assert!(matches!(
            parse_endpoint_url("https://10.0.0.5").unwrap().host_kind,
            EndpointHost::Ip(IpAddr::V4(_))
        ));
    }

    #[test]
    fn ip_literals_are_trusted_only_in_permitted_classes() {
        let resolver = FakeResolver::default();
        for endpoint in [
            "http://127.0.0.1:8080/v1",
            "http://10.1.2.3/v1",
            "http://172.16.0.9/v1",
            "http://192.168.5.5/v1",
            "http://[::1]:9000/v1",
            "https://[fd00::7]/v1",
            "https://[fc00::1]/v1",
        ] {
            assert!(
                check_endpoint_trust(endpoint, false, &resolver).is_ok(),
                "expected `{endpoint}` to be trusted"
            );
        }
    }

    #[test]
    fn public_ip_literals_are_rejected() {
        let resolver = FakeResolver::default();
        for endpoint in [
            "http://8.8.8.8/v1",
            "https://192.0.2.10/v1",
            "http://172.32.0.1/v1",
            "http://[2001:db8::1]/v1",
            "http://[fe80::1]/v1",
        ] {
            let err = check_endpoint_trust(endpoint, false, &resolver)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("non-permitted address"),
                "expected rejection of `{endpoint}`, got: {err}"
            );
        }
    }

    #[test]
    fn explicit_override_permits_public_endpoints() {
        let resolver = FakeResolver::default();
        let trusted = check_endpoint_trust("https://192.0.2.10/v1", true, &resolver).unwrap();
        assert!(trusted.detail.contains("override"));
    }

    #[test]
    fn hostnames_are_resolved_before_the_trust_decision() {
        let private = FakeResolver {
            ips: vec![v4([10, 1, 2, 3]), v6([0xfd00, 0, 0, 0, 0, 0, 0, 1])],
            fail: false,
        };
        let trusted =
            check_endpoint_trust("http://modelbox.internal:8000/v1", false, &private).unwrap();
        assert!(trusted.detail.contains("resolved to permitted addresses"));

        let mixed = FakeResolver {
            ips: vec![v4([10, 1, 2, 3]), v4([8, 8, 8, 8])],
            fail: false,
        };
        let err =
            check_endpoint_trust("http://modelbox.internal:8000/v1", false, &mixed).unwrap_err();
        assert!(
            matches!(err, EndpointTrustError::UntrustedAddress { .. }),
            "mixed public/private resolution must be rejected, got: {err}"
        );
    }

    #[test]
    fn unresolvable_hostnames_fail_closed() {
        let failing = FakeResolver {
            ips: vec![],
            fail: true,
        };
        assert!(matches!(
            check_endpoint_trust("http://missing.internal:1/v1", false, &failing),
            Err(EndpointTrustError::Resolution { .. })
        ));

        let empty = FakeResolver {
            ips: vec![],
            fail: false,
        };
        assert!(matches!(
            check_endpoint_trust("http://missing.internal:1/v1", false, &empty),
            Err(EndpointTrustError::NoAddresses { .. })
        ));
    }

    #[test]
    fn system_resolver_resolves_localhost_without_network() {
        let resolver = SystemResolver;
        let addresses = resolver.resolve("localhost").unwrap();
        assert!(!addresses.is_empty());
        assert!(
            addresses
                .iter()
                .all(|ip| matches!(classify_ip(*ip), AddressClass::Loopback))
        );
    }
}
