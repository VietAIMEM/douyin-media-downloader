//! Which destinations this application is willing to contact.
//!
//! Every URL the application requests is either typed by the user or produced by
//! a resolver reading a page whose JavaScript the application does not control.
//! The second kind is the reason this module exists: a media URL that arrives
//! from a payload, and every redirect hop a request follows, are destinations
//! nobody chose deliberately.
//!
//! # What is refused, and what is not
//!
//! Refused: loopback, private, link-local, shared, reserved and multicast
//! addresses, the unspecified address, and the special-use names (`localhost`,
//! `*.local`, `*.internal`, `*.home.arpa`). These are the destinations that mean
//! "somewhere on this machine or this local network" — a local admin interface,
//! a service that trusts its callers because it is only reachable locally, or a
//! cloud metadata endpoint. An accidental request to one of those is the whole
//! class of bug this prevents.
//!
//! **Not** refused: any public host. There is deliberately no allowlist. Douyin
//! serves its media from a wide set of CDN names that changes without notice,
//! and a list that must be updated to keep downloads working would be turned off
//! long before it caught anything. The policy is about *where* traffic may go,
//! not *which* hosts are legitimate — the latter is the resolver's job, and it
//! already refuses anything that is not a Douyin page.
//!
//! # What this does not do
//!
//! It inspects the URL, not the resolved address, so it does not defend against
//! DNS rebinding: a public name that resolves to a private address is allowed.
//! Defeating that needs the address pinned after resolution, which `reqwest`
//! does not expose. The check is therefore a guard against accident and
//! misdirection, not a hard security boundary, and it is documented as such
//! rather than described as comprehensive.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::errors::{AppError, AppResult};

/// Why a destination is not one this application will contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The string is not a URL at all.
    Unparseable,
    /// Not `http` or `https`.
    Scheme,
    /// The URL carries no host.
    NoHost,
    /// `127.0.0.0/8`, `::1` — this machine.
    Loopback,
    /// `10/8`, `172.16/12`, `192.168/16`, `fc00::/7` — the local network.
    Private,
    /// `169.254/16`, `fe80::/10` — including the cloud metadata address.
    LinkLocal,
    /// `100.64/10` — carrier-grade NAT, not a public destination.
    Shared,
    /// `240/4`, `255.255.255.255`, and the IPv6 equivalents.
    Reserved,
    /// `224/4`, `ff00::/8`.
    Multicast,
    /// `0.0.0.0`, `::`.
    Unspecified,
    /// A name reserved for the local machine or a local network.
    SpecialName,
    /// The name answered with both allowed and refused addresses.
    ///
    /// Refused outright rather than filtered. A name that answers with a public
    /// address *and* a loopback one is either misconfigured or hostile, and the
    /// two are indistinguishable from here. Keeping the public member and
    /// dropping the rest looks harmless until the mixed answer is the second
    /// half of a rebinding attack — the client would connect to whichever member
    /// it was handed, and "some member is public" is not the property that
    /// matters. "The address dialled was checked" is.
    MixedAnswer,
    /// The name did not resolve to anything.
    ///
    /// Refused rather than passed on as an empty iterator, which a connector
    /// would treat as a connect failure to be retried.
    EmptyAnswer,
}

impl Refusal {
    /// A short phrase for diagnostics. Never shown to the user verbatim.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Unparseable => "not a URL",
            Self::Scheme => "unsupported scheme",
            Self::NoHost => "no host",
            Self::Loopback => "loopback address",
            Self::Private => "private address",
            Self::LinkLocal => "link-local address",
            Self::Shared => "shared address space",
            Self::Reserved => "reserved address",
            Self::Multicast => "multicast address",
            Self::Unspecified => "unspecified address",
            Self::SpecialName => "local-only host name",
            Self::MixedAnswer => "mixed public and non-public addresses",
            Self::EmptyAnswer => "no resolved addresses",
        }
    }

    /// A stable machine tag, carried through the transport stack and read back
    /// by the error mapping.
    ///
    /// [`Self::describe`] is for a human reading a log line; this is for the
    /// round trip: a refusal happens inside the resolver, below the layer that
    /// builds application errors, and the only thing that survives the journey
    /// is a string. A tag is recoverable; a prose phrase is not, and guessing it
    /// back from prose is how a report ends up saying the wrong thing.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Unparseable => "unparseable",
            Self::Scheme => "scheme",
            Self::NoHost => "no-host",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Shared => "shared",
            Self::Reserved => "reserved",
            Self::Multicast => "multicast",
            Self::Unspecified => "unspecified",
            Self::SpecialName => "special-name",
            Self::MixedAnswer => "mixed-answer",
            Self::EmptyAnswer => "empty-answer",
        }
    }

    /// The refusal a `tag` names, if it is one of ours.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let refusal = match tag {
            "unparseable" => Self::Unparseable,
            "scheme" => Self::Scheme,
            "no-host" => Self::NoHost,
            "loopback" => Self::Loopback,
            "private" => Self::Private,
            "link-local" => Self::LinkLocal,
            "shared" => Self::Shared,
            "reserved" => Self::Reserved,
            "multicast" => Self::Multicast,
            "unspecified" => Self::Unspecified,
            "special-name" => Self::SpecialName,
            "mixed-answer" => Self::MixedAnswer,
            "empty-answer" => Self::EmptyAnswer,
            _ => return None,
        };
        Some(refusal)
    }
}

/// Whether `url` is a destination the application will contact.
///
/// Pure and allocation-light, so it can be called on every hop of a redirect
/// chain.
pub fn refusal(url: &str) -> Option<Refusal> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Some(Refusal::Unparseable);
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Some(Refusal::Scheme);
    }
    refusal_for_host(url_host(&parsed))
}

/// The host of a parsed URL, lowercased, with any trailing dot removed.
///
/// `example.com.` and `example.com` are the same name; treating them as
/// different would let a trailing dot walk past the name checks.
fn url_host(parsed: &url::Url) -> Option<String> {
    let host = parsed.host_str()?;
    let host = host.to_ascii_lowercase();
    Some(host.strip_suffix('.').unwrap_or(&host).to_string())
}

/// Whether a bare host is an acceptable destination.
pub fn refusal_for_host(host: Option<String>) -> Option<Refusal> {
    let Some(host) = host.filter(|host| !host.is_empty()) else {
        return Some(Refusal::NoHost);
    };

    // A literal address is checked arithmetically rather than by name.
    if let Ok(address) = host.parse::<IpAddr>() {
        return refusal_for_address(address);
    }

    // An IPv6 literal arrives bracketed from `host_str`? It does not — `url`
    // strips the brackets — but a hand-written host might still carry them.
    if let Ok(address) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        return refusal_for_address(address);
    }

    let special = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
        // A single label with no dot is a local name by DNS convention, but it
        // is also how an intranet host is written. Only the reserved suffixes
        // above are refused: refusing every dotless name would break nothing
        // real, but the reserved list is the part with a standard behind it.
        ;
    if special {
        return Some(Refusal::SpecialName);
    }
    None
}

/// Whether a resolved address is a destination the application will contact.
pub fn refusal_for_address(address: IpAddr) -> Option<Refusal> {
    match address {
        IpAddr::V4(v4) => refusal_for_v4(v4),
        IpAddr::V6(v6) => {
            // `::ffff:a.b.c.d` is an IPv4 destination wearing a v6 hat, so it is
            // judged by the IPv4 rules.
            //
            // Only the *mapped* form is unwrapped. `Ipv6Addr::to_ipv4` also
            // matches the IPv4-*compatible* range `::/96`, which contains `::1`
            // — unwrapping that would turn loopback into `0.0.0.1` and let the
            // single most important address in this list through.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return refusal_for_v4(v4);
            }
            refusal_for_v6(v6)
        }
    }
}

fn refusal_for_v4(address: Ipv4Addr) -> Option<Refusal> {
    let octets = address.octets();
    if address.is_unspecified() {
        return Some(Refusal::Unspecified);
    }
    if address.is_loopback() {
        return Some(Refusal::Loopback);
    }
    if address.is_private() {
        return Some(Refusal::Private);
    }
    if address.is_link_local() {
        return Some(Refusal::LinkLocal);
    }
    if address.is_multicast() {
        return Some(Refusal::Multicast);
    }
    if address.is_broadcast() {
        return Some(Refusal::Reserved);
    }
    // 100.64.0.0/10 — RFC 6598 shared address space.
    if octets[0] == 100 && (64..128).contains(&octets[1]) {
        return Some(Refusal::Shared);
    }
    // 240.0.0.0/4 — reserved, including 255.255.255.255 (handled above).
    if (240..=255).contains(&octets[0]) {
        return Some(Refusal::Reserved);
    }
    // 192.0.0.0/24 and 198.18.0.0/15 are not "public" in the sense that matters
    // here: both are special-use and never a real content host.
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        return Some(Refusal::Reserved);
    }
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return Some(Refusal::Reserved);
    }
    None
}

fn refusal_for_v6(address: Ipv6Addr) -> Option<Refusal> {
    let segments = address.segments();
    if address.is_unspecified() {
        return Some(Refusal::Unspecified);
    }
    if address.is_loopback() {
        return Some(Refusal::Loopback);
    }
    if address.is_multicast() {
        return Some(Refusal::Multicast);
    }
    // fc00::/7 — unique local addressing (the v6 analogue of a private range).
    if segments[0] & 0xfe00 == 0xfc00 {
        return Some(Refusal::Private);
    }
    // fe80::/10 — link-local.
    if segments[0] & 0xffc0 == 0xfe80 {
        return Some(Refusal::LinkLocal);
    }
    None
}

/// Refuse a destination that is not one this application contacts.
///
/// The message names the *kind* of destination rather than echoing the URL: the
/// URL may be a signed one, and the kind is the part that explains the refusal.
pub fn ensure_allowed(url: &str) -> AppResult<()> {
    let Some(refusal) = refusal(url) else {
        return Ok(());
    };
    Err(AppError::network(format!(
        "refusing a request to {kind}",
        kind = refusal.describe()
    ))
    .with_retryable(false)
    .with_user_message("That address is not a public destination.")
    .with_technical_detail(format!("destination={}", refusal.describe())))
}

/// Which destinations a client will contact.
///
/// A policy rather than a boolean so the permissive case is explicit and
/// greppable. The application ships [`DestinationPolicy::PublicOnly`]; the other
/// variant exists so a test driving its own loopback server can say so, and
/// there is nowhere it can be reached from the shipped application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationPolicy {
    /// Refuse loopback, private, link-local and special-use destinations.
    PublicOnly,
    /// Refuse nothing. For tests that talk to a server they started themselves.
    Any,
}

impl DestinationPolicy {
    /// Whether `url` may be contacted under this policy.
    pub fn allows(self, url: &str) -> bool {
        match self {
            Self::PublicOnly => refusal(url).is_none(),
            Self::Any => true,
        }
    }

    /// Refuse `url` with a typed error when this policy does not allow it.
    pub fn check(self, url: &str) -> AppResult<()> {
        match self {
            Self::PublicOnly => ensure_allowed(url),
            Self::Any => Ok(()),
        }
    }

    /// Classify a hostname before it is looked up.
    ///
    /// Checked first so a name reserved for the local machine is refused without
    /// asking a resolver about it — and so the decision does not depend on what
    /// that resolver would have answered.
    pub fn classify_name(self, host: &str) -> Option<Refusal> {
        match self {
            Self::PublicOnly => refusal_for_host(Some(host.to_ascii_lowercase())),
            Self::Any => None,
        }
    }

    /// Classify a whole DNS answer for one name.
    ///
    /// The unit of decision is the *answer*, not each address. An empty answer is
    /// refused rather than handed on as an empty iterator, which a connector
    /// would treat as a retryable connect failure. An answer that is entirely
    /// non-public is refused with the category of its addresses. An answer that
    /// mixes the two is refused as [`Refusal::MixedAnswer`] — see that variant
    /// for why the public members are not simply kept.
    pub fn classify_answer(self, addrs: &[SocketAddr]) -> Option<Refusal> {
        if self == Self::Any {
            return None;
        }
        if addrs.is_empty() {
            return Some(Refusal::EmptyAnswer);
        }
        let refusals: Vec<Refusal> = addrs
            .iter()
            .filter_map(|addr| refusal_for_address(addr.ip()))
            .collect();
        match refusals.len() {
            0 => None,
            // Every address is unusable, so which one is reported is a detail.
            n if n == addrs.len() => Some(refusals[0]),
            // Some usable, some not: the answer itself is not trustworthy.
            _ => Some(Refusal::MixedAnswer),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // --- destinations that must keep working --------------------------------

    #[test]
    fn public_douyin_destinations_are_allowed() {
        // The hosts the application actually talks to. A policy that broke any
        // of these would be turned off within a day.
        for url in [
            "https://www.douyin.com/video/7123456789012345678",
            "https://v.douyin.com/abc123/",
            "https://v3-cdn.douyin.com/aweme/v1/playwm/?video_id=v0200&token=SECRET",
            "https://v3-web.douyinvod.com/abc/def/video.mp4",
            "https://p3-cdn.douyin.com/img/cover.jpeg",
            "https://aweme.snssdk.com/aweme/v1/play/",
            "https://sf3-cdn.douyin.com/obj/tos-cn/test.mp4",
            "http://m.douyin.com/share/video/7123",
        ] {
            assert!(
                refusal(url).is_none(),
                "{url} must be allowed, got {:?}",
                refusal(url)
            );
        }
    }

    #[test]
    fn a_trailing_dot_does_not_slip_past_the_name_checks() {
        assert_eq!(refusal("http://localhost./x"), Some(Refusal::SpecialName));
        assert_eq!(refusal("http://foo.local./x"), Some(Refusal::SpecialName));
        assert!(refusal("http://www.douyin.com./video/1").is_none());
    }

    #[test]
    fn ordinary_public_addresses_are_allowed() {
        for url in [
            "https://8.8.8.8/x",
            "https://1.1.1.1/x",
            "https://91.198.174.192/x",
            "https://172.32.0.1/x",   // just outside 172.16/12
            "https://11.0.0.1/x",     // just outside 10/8
            "https://100.128.0.1/x",  // just outside 100.64/10
            "https://193.168.0.1/x",  // just outside 192.168/16
            "https://169.253.0.1/x",  // just outside 169.254/16
        ] {
            assert!(refusal(url).is_none(), "{url} must be allowed");
        }
    }

    // --- destinations that must be refused ----------------------------------

    #[test]
    fn loopback_is_refused_in_every_spelling() {
        for url in [
            "http://127.0.0.1:8080/admin",
            "http://127.1.2.3/",
            "http://localhost/",
            "http://localhost:5173/",
            "http://LOCALHOST/",
            "http://sub.localhost/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(
                matches!(
                    refusal(url),
                    Some(Refusal::Loopback | Refusal::SpecialName)
                ),
                "{url} must be refused, got {:?}",
                refusal(url)
            );
        }
    }

    #[test]
    fn private_and_link_local_ranges_are_refused() {
        for (url, expected) in [
            ("http://10.0.0.1/", Refusal::Private),
            ("http://10.255.255.254/", Refusal::Private),
            ("http://172.16.0.1/", Refusal::Private),
            ("http://172.31.255.255/", Refusal::Private),
            ("http://192.168.1.1/", Refusal::Private),
            ("http://[fc00::1]/", Refusal::Private),
            ("http://[fd12:3456::1]/", Refusal::Private),
            ("http://169.254.169.254/latest/meta-data/", Refusal::LinkLocal),
            ("http://[fe80::1]/", Refusal::LinkLocal),
        ] {
            assert_eq!(refusal(url), Some(expected), "{url}");
        }
    }

    #[test]
    fn the_cloud_metadata_address_is_refused() {
        // The one that turns an SSRF into a credential leak.
        assert_eq!(
            refusal("http://169.254.169.254/latest/meta-data/iam/security-credentials/"),
            Some(Refusal::LinkLocal)
        );
    }

    #[test]
    fn reserved_and_shared_ranges_are_refused() {
        for (url, expected) in [
            ("http://0.0.0.0/", Refusal::Unspecified),
            ("http://[::]/", Refusal::Unspecified),
            ("http://100.64.0.1/", Refusal::Shared),
            ("http://100.127.255.255/", Refusal::Shared),
            ("http://240.0.0.1/", Refusal::Reserved),
            ("http://255.255.255.255/", Refusal::Reserved),
            ("http://224.0.0.1/", Refusal::Multicast),
            ("http://[ff02::1]/", Refusal::Multicast),
            ("http://192.0.0.1/", Refusal::Reserved),
            ("http://198.18.0.1/", Refusal::Reserved),
        ] {
            assert_eq!(refusal(url), Some(expected), "{url}");
        }
    }

    #[test]
    fn local_only_names_are_refused() {
        for url in [
            "http://printer.local/",
            "http://nas.internal/",
            "http://router.home.arpa/",
        ] {
            assert_eq!(refusal(url), Some(Refusal::SpecialName), "{url}");
        }
    }

    #[test]
    fn non_http_schemes_are_refused() {
        for url in [
            "file:///C:/Windows/System32/config/SAM",
            "ftp://example.com/x",
            "data:text/html,<script>",
            "javascript:alert(1)",
            "blob:https://www.douyin.com/abc",
        ] {
            assert!(
                matches!(refusal(url), Some(Refusal::Scheme | Refusal::Unparseable)),
                "{url} must be refused, got {:?}",
                refusal(url)
            );
        }
    }

    #[test]
    fn a_malformed_url_is_refused_rather_than_guessed_at() {
        assert_eq!(refusal("not a url"), Some(Refusal::Unparseable));
        assert_eq!(refusal(""), Some(Refusal::Unparseable));
        // `http://` carries no host at all, which the URL parser itself rejects.
        assert_eq!(refusal("http://"), Some(Refusal::Unparseable));
        assert_eq!(refusal("douyin.com/video/1"), Some(Refusal::Unparseable));
    }

    #[test]
    fn the_refusal_error_is_typed_and_says_nothing_about_the_url() {
        let err = ensure_allowed("http://127.0.0.1:8080/admin?token=SECRET").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Network);
        assert!(!err.is_retryable(), "a refused destination is not transient");
        assert!(
            !err.technical_detail().unwrap_or_default().contains("SECRET"),
            "the detail must not carry the URL"
        );
        assert!(!err.user_message().is_empty());
    }

    #[test]
    fn the_permissive_policy_is_explicit_and_allows_loopback() {
        // What the tests that drive their own local server rely on.
        assert!(DestinationPolicy::Any.allows("http://127.0.0.1:9/x"));
        assert!(!DestinationPolicy::PublicOnly.allows("http://127.0.0.1:9/x"));
        assert!(DestinationPolicy::PublicOnly.allows("https://www.douyin.com/x"));
        assert!(DestinationPolicy::Any.check("http://127.0.0.1:9/x").is_ok());
        assert!(DestinationPolicy::PublicOnly
            .check("http://127.0.0.1:9/x")
            .is_err());
    }
}
