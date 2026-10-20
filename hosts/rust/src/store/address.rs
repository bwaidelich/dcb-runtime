//! What follows the scheme in a store URL, for the adapters that read
//! more than a host from it.

/// `[user@]host:port[/path][?query]`.
pub struct Address<'a> {
    pub user: Option<&'a str>,
    pub host: &'a str,
    /// Without its leading slash; empty when there is none.
    pub path: &'a str,
    #[cfg_attr(not(feature = "eventsourcingdb"), allow(dead_code))]
    query: &'a str,
}

impl<'a> Address<'a> {
    pub fn parse(address: &'a str) -> Self {
        let (rest, query) = address.split_once('?').unwrap_or((address, ""));
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, authority),
        };
        Address { user, host, path: path.trim_end_matches('/'), query }
    }

    #[cfg_attr(not(feature = "eventsourcingdb"), allow(dead_code))]
    pub fn param(&self, name: &str) -> Option<&'a str> {
        self.query.split('&').find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    }
}
