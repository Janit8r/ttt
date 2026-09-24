use std::{net::SocketAddr, time::Duration};
use url::Url;

pub const DEFAULT_REFERER: &str = "https://missav.ws/dm242/cn";
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub bind_addr: SocketAddr,
    pub public_base: Option<String>,
    pub workers: usize,
    pub max_playlist_bytes: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
    pub referer: String,
}

impl ProxyConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::args().skip(1), |name| std::env::var(name).ok())
    }

    fn parse(args: impl Iterator<Item = String>, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let bind_addr: SocketAddr = env("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse().map_err(|_| "BIND_ADDR must be an IP:port socket address")?;
        if bind_addr.port() == 0 { return Err("BIND_ADDR port must not be zero".into()); }
        let mut args = args;
        let mut local_ip = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-Li" => local_ip = Some(args.next().ok_or("-Li requires an address")?),
                _ => return Err(format!("Unknown option: {arg}; supported: -Li <address>")),
            }
        }
        let public_base = match env("PUBLIC_BASE_URL") {
            Some(base) => Some(validate_base(&base)?),
            None => local_ip.or_else(|| env("LOCAL_IP")).map(|host| {
                let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
                validate_base(&format!("http://{host}:{}/", bind_addr.port()))
            }).transpose()?,
        };
        let number = |name: &str, default: usize, max: usize| -> Result<usize, String> {
            match env(name) {
                None => Ok(default),
                Some(s) => s.parse::<usize>().ok().filter(|n| *n > 0 && *n <= max)
                    .ok_or_else(|| format!("{name} must be between 1 and {max}")),
            }
        };
        let user_agent = env("UPSTREAM_USER_AGENT").unwrap_or_else(|| DEFAULT_USER_AGENT.into());
        let referer = env("UPSTREAM_REFERER").unwrap_or_else(|| DEFAULT_REFERER.into());
        for value in [&user_agent, &referer] {
            http::HeaderValue::from_str(value).map_err(|_| "Invalid upstream header configuration")?;
        }
        Ok(Self {
            bind_addr,
            public_base,
            workers: number("WORKERS", std::thread::available_parallelism().map(usize::from).unwrap_or(1), 256)?,
            max_playlist_bytes: number("MAX_PLAYLIST_BYTES", 8 * 1024 * 1024, 64 * 1024 * 1024)?,
            connect_timeout: Duration::from_secs(number("CONNECT_TIMEOUT_SECS", 10, 300)? as u64),
            read_timeout: Duration::from_secs(number("READ_TIMEOUT_SECS", 30, 3600)? as u64),
            user_agent,
            referer,
        })
    }

    /// Host is used only for advertised links, never for selecting the upstream.
    /// Behind a TLS terminator set PUBLIC_BASE_URL; forwarded headers are not trusted.
    pub fn public_base_for(&self, authority: Option<&str>) -> Result<String, String> {
        if let Some(base) = &self.public_base { return Ok(base.clone()); }
        let authority = authority.ok_or("Missing Host; configure PUBLIC_BASE_URL")?;
        let parsed: http::uri::Authority = authority.parse().map_err(|_| "Invalid Host")?;
        if parsed.host().is_empty() || authority.contains('@') { return Err("Invalid Host".into()); }
        validate_base(&format!("http://{authority}/"))
    }
}

fn validate_base(base: &str) -> Result<String, String> {
    let url = Url::parse(base).map_err(|_| "Invalid PUBLIC_BASE_URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Public base must be an HTTP(S) URL without credentials, query or fragment".into());
    }
    let mut base = url.to_string();
    if !base.ends_with('/') { base.push('/'); }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(pairs: &[(&str, &str)]) -> Result<ProxyConfig, String> {
        ProxyConfig::parse(std::iter::empty(), |key| pairs.iter().find(|p| p.0 == key).map(|p| p.1.to_string()))
    }
    #[test]
    fn default_advertises_client_visible_host() {
        let cfg = config(&[]).unwrap();
        assert_eq!(cfg.public_base_for(Some("169.254.155.1:8080")).unwrap(), "http://169.254.155.1:8080/");
        assert!(cfg.public_base_for(None).is_err());
    }
    #[test]
    fn ipv6_and_explicit_public_url() {
        let cfg = config(&[("BIND_ADDR", "[::1]:9000"), ("LOCAL_IP", "::1")]).unwrap();
        assert_eq!(cfg.public_base_for(None).unwrap(), "http://[::1]:9000/");
        let cfg = config(&[("PUBLIC_BASE_URL", "https://proxy.example/hls"), ("LOCAL_IP", "192.0.2.1")]).unwrap();
        assert_eq!(cfg.public_base_for(Some("ignored")).unwrap(), "https://proxy.example/hls/");
    }
    #[test]
    fn rejects_invalid_configuration() {
        for pairs in [vec![("WORKERS", "0")], vec![("BIND_ADDR", "bad")], vec![("MAX_PLAYLIST_BYTES", "-1")], vec![("PUBLIC_BASE_URL", "https://user:password@proxy.example/")]] {
            assert!(config(&pairs).is_err());
        }
        assert!(ProxyConfig::parse(vec!["-Li".into()].into_iter(), |_| None).is_err());
    }
}
