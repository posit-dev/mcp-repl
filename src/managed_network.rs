use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
#[cfg(target_os = "linux")]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::sandbox::ManagedNetworkPolicy;

// Server-owned proxy routing with OS sandbox egress limited to loopback proxy
// ports. Matching is host-only because HTTPS CONNECT does not expose URL paths.
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const LISTENER_IDLE_SLEEP: Duration = Duration::from_millis(20);
pub(crate) const PROXY_ACTIVE_ENV_KEY: &str = "MCP_REPL_MANAGED_NETWORK_PROXY_ACTIVE";
const DEFAULT_NO_PROXY_VALUE: &str =
    "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";

const HTTP_PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "npm_config_http_proxy",
    "npm_config_https_proxy",
    "npm_config_proxy",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ws_proxy",
    "wss_proxy",
];
const SOCKS_PROXY_ENV_KEYS: &[&str] = &["ALL_PROXY", "all_proxy", "FTP_PROXY", "ftp_proxy"];
const NO_PROXY_ENV_KEYS: &[&str] = &["NO_PROXY", "no_proxy", "npm_config_noproxy"];
#[cfg(target_os = "linux")]
pub(crate) const LINUX_MANAGED_NETWORK_HTTP_BRIDGE_PORT: u16 = 39080;
#[cfg(target_os = "linux")]
pub(crate) const LINUX_MANAGED_NETWORK_SOCKS_BRIDGE_PORT: u16 = 39081;
#[cfg(target_os = "linux")]
const LINUX_MANAGED_NETWORK_HTTP_SOCKET_NAME: &str = "mn-http.sock";
#[cfg(target_os = "linux")]
const LINUX_MANAGED_NETWORK_SOCKS_SOCKET_NAME: &str = "mn-socks.sock";

#[derive(Debug)]
pub enum ManagedNetworkError {
    InvalidDomainPattern(String),
    Io(io::Error),
}

impl std::fmt::Display for ManagedNetworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDomainPattern(message) => f.write_str(message),
            Self::Io(err) => write!(f, "managed network proxy I/O error: {err}"),
        }
    }
}

impl std::error::Error for ManagedNetworkError {}

impl From<io::Error> for ManagedNetworkError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedProxyConfig {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub allow_local_binding: bool,
}

impl ManagedProxyConfig {
    pub fn from_policy(policy: &ManagedNetworkPolicy) -> Result<Self, ManagedNetworkError> {
        validate_domain_patterns(
            "permissions.network.allowed_domains",
            &policy.allowed_domains,
        )
        .map_err(ManagedNetworkError::InvalidDomainPattern)?;
        validate_domain_patterns("permissions.network.denied_domains", &policy.denied_domains)
            .map_err(ManagedNetworkError::InvalidDomainPattern)?;
        Ok(Self {
            allowed_domains: policy.allowed_domains.clone(),
            denied_domains: policy.denied_domains.clone(),
            allow_local_binding: policy.allow_local_binding,
        })
    }
}

#[derive(Debug, Clone)]
enum DomainPattern {
    Exact(String),
    SubdomainsOnly(String),
    ApexAndSubdomains(String),
}

impl DomainPattern {
    fn parse(raw: &str) -> Result<Self, String> {
        validate_domain_pattern(raw)?;
        let normalized = normalize_host(raw);
        if let Some(domain) = normalized.strip_prefix("**.") {
            Ok(Self::ApexAndSubdomains(domain.to_string()))
        } else if let Some(domain) = normalized.strip_prefix("*.") {
            Ok(Self::SubdomainsOnly(domain.to_string()))
        } else {
            Ok(Self::Exact(normalized))
        }
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(pattern) => host == pattern,
            Self::SubdomainsOnly(domain) => {
                host.len() > domain.len()
                    && host.ends_with(domain)
                    && host.as_bytes().get(host.len() - domain.len() - 1) == Some(&b'.')
            }
            Self::ApexAndSubdomains(domain) => {
                host == domain
                    || (host.len() > domain.len()
                        && host.ends_with(domain)
                        && host.as_bytes().get(host.len() - domain.len() - 1) == Some(&b'.'))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct HostPolicy {
    allowed: Vec<DomainPattern>,
    denied: Vec<DomainPattern>,
    allow_local_binding: bool,
}

impl HostPolicy {
    fn new(config: &ManagedProxyConfig) -> Result<Self, ManagedNetworkError> {
        let allowed = config
            .allowed_domains
            .iter()
            .map(|pattern| DomainPattern::parse(pattern))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ManagedNetworkError::InvalidDomainPattern)?;
        let denied = config
            .denied_domains
            .iter()
            .map(|pattern| DomainPattern::parse(pattern))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ManagedNetworkError::InvalidDomainPattern)?;
        Ok(Self {
            allowed,
            denied,
            allow_local_binding: config.allow_local_binding,
        })
    }

    fn allows(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if host.is_empty() || host_contains_control_chars(&host) {
            return false;
        }
        if self.denied.iter().any(|pattern| pattern.matches(&host)) {
            return false;
        }
        if !self.allow_local_binding && is_local_or_private_host(&host) {
            return false;
        }
        !self.allowed.is_empty() && self.allowed.iter().any(|pattern| pattern.matches(&host))
    }

    fn allows_socket_addr(&self, addr: SocketAddr) -> bool {
        !is_non_public_ip(addr.ip()) || (self.allow_local_binding && is_loopback_ip(addr.ip()))
    }
}

pub struct ManagedNetworkProxy {
    config: ManagedProxyConfig,
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    listener_threads: Vec<thread::JoinHandle<()>>,
    #[cfg(target_os = "linux")]
    linux_relay: Mutex<Option<LinuxManagedNetworkRelay>>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinuxManagedNetworkBridgeConfig {
    pub(crate) http_socket: PathBuf,
    pub(crate) socks_socket: PathBuf,
}

#[cfg(target_os = "linux")]
struct LinuxManagedNetworkRelay {
    config: LinuxManagedNetworkBridgeConfig,
    shutdown: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl ManagedNetworkProxy {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn start(config: ManagedProxyConfig) -> Result<Self, ManagedNetworkError> {
        Self::start_with_addrs(
            config,
            SocketAddr::from(([127, 0, 0, 1], 0)),
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
    }

    #[cfg(target_os = "windows")]
    pub fn start_on_loopback_ports(
        config: ManagedProxyConfig,
        http_port: u16,
        socks_port: u16,
    ) -> Result<Self, ManagedNetworkError> {
        Self::start_with_addrs(
            config,
            SocketAddr::from(([127, 0, 0, 1], http_port)),
            SocketAddr::from(([127, 0, 0, 1], socks_port)),
        )
    }

    fn start_with_addrs(
        config: ManagedProxyConfig,
        http_bind_addr: SocketAddr,
        socks_bind_addr: SocketAddr,
    ) -> Result<Self, ManagedNetworkError> {
        let policy = Arc::new(HostPolicy::new(&config)?);
        let http_listener = TcpListener::bind(http_bind_addr)?;
        let socks_listener = TcpListener::bind(socks_bind_addr)?;
        let http_addr = http_listener.local_addr()?;
        let socks_addr = socks_listener.local_addr()?;
        http_listener.set_nonblocking(true)?;
        socks_listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let listener_threads = vec![
            spawn_http_listener(http_listener, Arc::clone(&policy), Arc::clone(&shutdown)),
            spawn_socks_listener(socks_listener, policy, Arc::clone(&shutdown)),
        ];

        Ok(Self {
            config,
            http_addr,
            socks_addr,
            shutdown,
            listener_threads,
            #[cfg(target_os = "linux")]
            linux_relay: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &ManagedProxyConfig {
        &self.config
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    pub fn socks_addr(&self) -> SocketAddr {
        self.socks_addr
    }

    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn apply_to_env(&self, env: &mut HashMap<String, String>) {
        let http_proxy_url = format!("http://{}", self.http_addr);
        let socks_proxy_url = format!("socks5h://{}", self.socks_addr);
        env.insert(PROXY_ACTIVE_ENV_KEY.to_string(), "1".to_string());
        set_env_keys(env, HTTP_PROXY_ENV_KEYS, &http_proxy_url);
        set_env_keys(env, SOCKS_PROXY_ENV_KEYS, &socks_proxy_url);
        set_env_keys(env, NO_PROXY_ENV_KEYS, DEFAULT_NO_PROXY_VALUE);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn apply_linux_bridge_to_env(
        &self,
        env: &mut HashMap<String, String>,
        session_temp_dir: &Path,
    ) -> Result<LinuxManagedNetworkBridgeConfig, ManagedNetworkError> {
        let bridge = self.ensure_linux_relay(session_temp_dir)?;
        env.insert(PROXY_ACTIVE_ENV_KEY.to_string(), "1".to_string());
        set_env_keys(
            env,
            HTTP_PROXY_ENV_KEYS,
            &format!("http://127.0.0.1:{LINUX_MANAGED_NETWORK_HTTP_BRIDGE_PORT}"),
        );
        set_env_keys(
            env,
            SOCKS_PROXY_ENV_KEYS,
            &format!("socks5h://127.0.0.1:{LINUX_MANAGED_NETWORK_SOCKS_BRIDGE_PORT}"),
        );
        if self.config.allow_local_binding {
            set_env_keys(env, NO_PROXY_ENV_KEYS, "");
        } else {
            set_env_keys(env, NO_PROXY_ENV_KEYS, DEFAULT_NO_PROXY_VALUE);
        }
        Ok(bridge)
    }

    #[cfg(target_os = "linux")]
    fn ensure_linux_relay(
        &self,
        session_temp_dir: &Path,
    ) -> Result<LinuxManagedNetworkBridgeConfig, ManagedNetworkError> {
        let mut relay = self
            .linux_relay
            .lock()
            .map_err(|_| io::Error::other("managed network relay mutex poisoned"))?;
        let expected = LinuxManagedNetworkBridgeConfig {
            http_socket: session_temp_dir.join(LINUX_MANAGED_NETWORK_HTTP_SOCKET_NAME),
            socks_socket: session_temp_dir.join(LINUX_MANAGED_NETWORK_SOCKS_SOCKET_NAME),
        };
        let needs_new = relay.as_ref().is_none_or(|existing| {
            existing.config != expected
                || !existing.config.http_socket.exists()
                || !existing.config.socks_socket.exists()
        });
        if needs_new {
            *relay = None;
            *relay = Some(LinuxManagedNetworkRelay::start(
                expected,
                self.http_addr,
                self.socks_addr,
            )?);
        }
        Ok(relay
            .as_ref()
            .expect("relay should exist after start")
            .config
            .clone())
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn route_local_targets_through_proxy(env: &mut HashMap<String, String>) {
        set_env_keys(env, NO_PROXY_ENV_KEYS, "");
    }
}

impl Drop for ManagedNetworkProxy {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Ok(mut relay) = self.linux_relay.lock() {
            *relay = None;
        }
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.http_addr);
        let _ = TcpStream::connect(self.socks_addr);
        for handle in self.listener_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(target_os = "linux")]
impl LinuxManagedNetworkRelay {
    fn start(
        config: LinuxManagedNetworkBridgeConfig,
        http_addr: SocketAddr,
        socks_addr: SocketAddr,
    ) -> Result<Self, ManagedNetworkError> {
        if let Some(parent) = config.http_socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_stale_unix_socket(&config.http_socket)?;
        remove_stale_unix_socket(&config.socks_socket)?;
        let http_listener = UnixListener::bind(&config.http_socket)?;
        let socks_listener = UnixListener::bind(&config.socks_socket)?;
        http_listener.set_nonblocking(true)?;
        socks_listener.set_nonblocking(true)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let threads = vec![
            spawn_unix_relay_listener(http_listener, http_addr, Arc::clone(&shutdown)),
            spawn_unix_relay_listener(socks_listener, socks_addr, Arc::clone(&shutdown)),
        ];

        Ok(Self {
            config,
            shutdown,
            threads,
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxManagedNetworkRelay {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.config.http_socket);
        let _ = UnixStream::connect(&self.config.socks_socket);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.config.http_socket);
        let _ = std::fs::remove_file(&self.config.socks_socket);
    }
}

#[cfg(target_os = "linux")]
fn remove_stale_unix_socket(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "linux")]
fn spawn_unix_relay_listener(
    listener: UnixListener,
    upstream_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    thread::spawn(move || handle_unix_relay_client(stream, upstream_addr));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(LISTENER_IDLE_SLEEP);
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(target_os = "linux")]
fn handle_unix_relay_client(client: UnixStream, upstream_addr: SocketAddr) {
    let Ok(upstream) = TcpStream::connect(upstream_addr) else {
        return;
    };
    let _ = proxy_unix_to_tcp(client, upstream);
}

#[cfg(target_os = "linux")]
fn proxy_unix_to_tcp(client: UnixStream, upstream: TcpStream) -> io::Result<()> {
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let client_to_upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });

    let mut upstream_read = upstream;
    let mut client_write = client;
    let result = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = client_to_upstream.join();
    result.map(|_| ())
}

fn set_env_keys(env: &mut HashMap<String, String>, keys: &[&str], value: &str) {
    for key in keys {
        env.insert((*key).to_string(), value.to_string());
    }
}

pub fn validate_domain_patterns(label: &str, patterns: &[String]) -> Result<(), String> {
    for pattern in patterns {
        validate_domain_pattern(pattern)
            .map_err(|err| format!("{label} entries must be host patterns: {err}"))?;
    }
    Ok(())
}

fn validate_domain_pattern(raw: &str) -> Result<(), String> {
    let pattern = raw.trim();
    if pattern.is_empty() {
        return Err("empty pattern".to_string());
    }
    if pattern.contains("://") || pattern.contains('/') || pattern.contains(':') {
        return Err(format!(
            "{pattern:?} is not supported; use a host pattern like pypi.org or *.example.com"
        ));
    }
    if pattern == "*" {
        return Err("global wildcard \"*\" is not supported".to_string());
    }

    let domain = if let Some(domain) = pattern.strip_prefix("**.") {
        domain
    } else if let Some(domain) = pattern.strip_prefix("*.") {
        domain
    } else {
        pattern
    };

    if domain.contains('*') {
        return Err(format!("{pattern:?} contains an unsupported wildcard"));
    }
    if domain.eq_ignore_ascii_case("localhost") || domain.parse::<Ipv4Addr>().is_ok() {
        return Ok(());
    }
    validate_dns_name(domain).map_err(|err| format!("{pattern:?}: {err}"))
}

fn validate_dns_name(domain: &str) -> Result<(), &'static str> {
    if !domain.contains('.') {
        return Err("domain must contain at least one dot");
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return Err("domain must not start or end with a dot");
    }
    for part in domain.split('.') {
        if part.is_empty() {
            return Err("domain labels must not be empty");
        }
        if part.starts_with('-') || part.ends_with('-') {
            return Err("domain labels must not start or end with '-'");
        }
        if !part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("domain labels may contain only letters, digits, and '-'");
        }
    }
    Ok(())
}

fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.starts_with('[') && host.ends_with(']') && host.len() > 2 {
        host[1..host.len() - 1].to_string()
    } else {
        host
    }
}

fn host_contains_control_chars(host: &str) -> bool {
    host.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
}

fn is_local_or_private_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => is_non_public_ip(ip),
        Err(_) => false,
    }
}

fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_non_public_ipv4(ip),
        IpAddr::V6(ip) => is_non_public_ipv6(ip),
    }
}

fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ipv4_mapped_from_ipv6(ip).is_some_and(|mapped| mapped.is_loopback()) || ip.is_loopback()
        }
    }
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240
}

fn is_non_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ipv4_mapped_from_ipv6(ip) {
        return is_non_public_ipv4(mapped);
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
}

fn ipv4_mapped_from_ipv6(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let octets = ip.octets();
    if octets[..10] == [0; 10] && octets[10] == 0xff && octets[11] == 0xff {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    }
}

fn spawn_http_listener(
    listener: TcpListener,
    policy: Arc<HostPolicy>,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || listener_loop(listener, policy, shutdown, handle_http_client))
}

fn spawn_socks_listener(
    listener: TcpListener,
    policy: Arc<HostPolicy>,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || listener_loop(listener, policy, shutdown, handle_socks_client))
}

fn listener_loop(
    listener: TcpListener,
    policy: Arc<HostPolicy>,
    shutdown: Arc<AtomicBool>,
    handler: fn(TcpStream, Arc<HostPolicy>),
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let policy = Arc::clone(&policy);
                thread::spawn(move || handler(stream, policy));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(LISTENER_IDLE_SLEEP);
            }
            Err(_) => break,
        }
    }
}

fn handle_http_client(mut client: TcpStream, policy: Arc<HostPolicy>) {
    if let Err(err) = handle_http_client_impl(&mut client, policy) {
        let _ = write_http_error(&mut client, 502, &format!("proxy error: {err}"));
    }
}

fn handle_http_client_impl(client: &mut TcpStream, policy: Arc<HostPolicy>) -> io::Result<()> {
    let header = read_http_header(client)?;
    let first_line_end = header
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let first_line = std::str::from_utf8(&header[..first_line_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request line is not UTF-8"))?;
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP method"))?;
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP target"))?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = parse_host_port(target, 443) else {
            return write_http_error(client, 400, "invalid CONNECT target");
        };
        if !policy.allows(&host) {
            return write_http_error(client, 403, "Connection blocked by network allowlist");
        }
        let Some(upstream) = connect_upstream(&host, port, policy.as_ref())? else {
            return write_http_error(client, 403, "Connection blocked by network allowlist");
        };
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        proxy_bidirectional(client.try_clone()?, upstream)?;
        return Ok(());
    }

    let Some(absolute) = parse_absolute_http_target(target) else {
        return write_http_error(client, 400, "expected absolute HTTP proxy request target");
    };
    if !policy.allows(&absolute.host) {
        return write_http_error(client, 403, "Connection blocked by network allowlist");
    }
    if !host_headers_match_target(&header[first_line_end + 2..], &absolute.host, absolute.port) {
        return write_http_error(client, 403, "Host header does not match proxy target");
    }

    let Some(mut upstream) = connect_upstream(&absolute.host, absolute.port, policy.as_ref())?
    else {
        return write_http_error(client, 403, "Connection blocked by network allowlist");
    };
    let header_tail = &header[first_line_end + 2..];
    if header_has_name(header_tail, b"transfer-encoding") {
        return write_http_error(
            client,
            400,
            "chunked HTTP proxy request bodies are not supported",
        );
    }
    let content_length = content_length_from_headers(header_tail)?;
    write_one_request_http_header(&mut upstream, method, &absolute.path, version, header_tail)?;
    copy_exact_bytes(client, &mut upstream, content_length)?;
    let _ = upstream.shutdown(Shutdown::Write);
    relay_plain_http_response(client, upstream)?;
    Ok(())
}

fn read_http_header(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while header.len() < MAX_HTTP_HEADER_BYTES {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP headers completed",
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "HTTP headers exceeded proxy limit",
    ))
}

struct AbsoluteHttpTarget {
    host: String,
    port: u16,
    path: String,
}

fn parse_absolute_http_target(target: &str) -> Option<AbsoluteHttpTarget> {
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = parse_host_port(authority, 80)?;
    Some(AbsoluteHttpTarget {
        host,
        port,
        path: path.to_string(),
    })
}

fn parse_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = &authority[1..end];
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            Some((host.to_string(), port.parse::<u16>().ok()?))
        }
        _ if !authority.is_empty() => Some((authority.to_string(), default_port)),
        _ => None,
    }
}

fn connect_upstream(host: &str, port: u16, policy: &HostPolicy) -> io::Result<Option<TcpStream>> {
    let addrs = (host, port).to_socket_addrs()?;
    let mut last_err = None;
    for addr in addrs {
        if !policy.allows_socket_addr(addr) {
            continue;
        }
        match TcpStream::connect(addr) {
            Ok(stream) => return Ok(Some(stream)),
            Err(err) => last_err = Some(err),
        }
    }
    match last_err {
        Some(err) => Err(err),
        None => Ok(None),
    }
}

fn host_headers_match_target(header_tail: &[u8], target_host: &str, target_port: u16) -> bool {
    for line in header_tail.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        if !line[..colon].eq_ignore_ascii_case(b"host") {
            continue;
        }
        let value = trim_ascii_bytes(&line[colon + 1..]);
        let Ok(authority) = std::str::from_utf8(value) else {
            return false;
        };
        let Some((host, port)) = parse_host_port(authority, target_port) else {
            return false;
        };
        if normalize_host(&host) != normalize_host(target_host) || port != target_port {
            return false;
        }
    }
    true
}

fn write_one_request_http_header(
    upstream: &mut TcpStream,
    method: &str,
    path: &str,
    version: &str,
    header_tail: &[u8],
) -> io::Result<()> {
    upstream.write_all(format!("{method} {path} {version}\r\n").as_bytes())?;
    let mut wrote_connection_close = false;
    for line in header_tail.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if header_line_has_name(line, b"proxy-connection") {
            continue;
        }
        if header_line_has_name(line, b"connection") {
            if !wrote_connection_close {
                upstream.write_all(b"Connection: close\r\n")?;
                wrote_connection_close = true;
            }
            continue;
        }
        upstream.write_all(line)?;
        upstream.write_all(b"\r\n")?;
    }
    if !wrote_connection_close {
        upstream.write_all(b"Connection: close\r\n")?;
    }
    upstream.write_all(b"\r\n")
}

fn content_length_from_headers(header_tail: &[u8]) -> io::Result<u64> {
    let mut content_length = None;
    for line in header_tail.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(value) = header_line_value(line, b"content-length") else {
            continue;
        };
        let value = std::str::from_utf8(trim_ascii_bytes(value)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Content-Length is not UTF-8")
        })?;
        let value = value
            .parse::<u64>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Content-Length is invalid"))?;
        if content_length.is_some_and(|previous| previous != value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "conflicting Content-Length headers",
            ));
        }
        content_length = Some(value);
    }
    Ok(content_length.unwrap_or(0))
}

fn header_has_name(header_tail: &[u8], name: &[u8]) -> bool {
    header_tail
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .take_while(|line| !line.is_empty())
        .any(|line| header_line_has_name(line, name))
}

fn header_line_has_name(line: &[u8], name: &[u8]) -> bool {
    header_line_value(line, name).is_some()
}

fn header_line_value<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    if trim_ascii_bytes(&line[..colon]).eq_ignore_ascii_case(name) {
        Some(&line[colon + 1..])
    } else {
        None
    }
}

fn copy_exact_bytes(
    reader: &mut TcpStream,
    writer: &mut TcpStream,
    mut bytes: u64,
) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    while bytes > 0 {
        let chunk_size = buffer.len().min(bytes as usize);
        reader.read_exact(&mut buffer[..chunk_size])?;
        writer.write_all(&buffer[..chunk_size])?;
        bytes -= chunk_size as u64;
    }
    Ok(())
}

fn relay_plain_http_response(client: &mut TcpStream, mut upstream: TcpStream) -> io::Result<()> {
    let result = io::copy(&mut upstream, &mut *client);
    let _ = client.shutdown(Shutdown::Write);
    result.map(|_| ())
}

fn trim_ascii_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn write_http_error(stream: &mut TcpStream, status: u16, message: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        502 => "Bad Gateway",
        _ => "Error",
    };
    stream.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        )
        .as_bytes(),
    )
}

fn handle_socks_client(mut client: TcpStream, policy: Arc<HostPolicy>) {
    let _ = handle_socks_client_impl(&mut client, policy);
}

fn handle_socks_client_impl(client: &mut TcpStream, policy: Arc<HostPolicy>) -> io::Result<()> {
    let mut greeting = [0_u8; 2];
    client.read_exact(&mut greeting)?;
    if greeting[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported SOCKS version",
        ));
    }
    let mut methods = vec![0_u8; greeting[1] as usize];
    client.read_exact(&mut methods)?;
    client.write_all(&[5, 0])?;

    let mut head = [0_u8; 4];
    client.read_exact(&mut head)?;
    if head[0] != 5 || head[1] != 1 {
        write_socks_reply(client, 7)?;
        return Ok(());
    }

    let host = match head[3] {
        1 => {
            let mut octets = [0_u8; 4];
            client.read_exact(&mut octets)?;
            Ipv4Addr::from(octets).to_string()
        }
        3 => {
            let mut len = [0_u8; 1];
            client.read_exact(&mut len)?;
            let mut bytes = vec![0_u8; len[0] as usize];
            client.read_exact(&mut bytes)?;
            String::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "SOCKS domain is not UTF-8")
            })?
        }
        4 => {
            let mut octets = [0_u8; 16];
            client.read_exact(&mut octets)?;
            Ipv6Addr::from(octets).to_string()
        }
        _ => {
            write_socks_reply(client, 8)?;
            return Ok(());
        }
    };
    let mut port_bytes = [0_u8; 2];
    client.read_exact(&mut port_bytes)?;
    let port = u16::from_be_bytes(port_bytes);

    if !policy.allows(&host) {
        write_socks_reply(client, 2)?;
        return Ok(());
    }

    match connect_upstream(&host, port, policy.as_ref()) {
        Ok(Some(upstream)) => {
            write_socks_reply(client, 0)?;
            proxy_bidirectional(client.try_clone()?, upstream)?;
        }
        Ok(None) => {
            write_socks_reply(client, 2)?;
        }
        Err(_) => {
            write_socks_reply(client, 4)?;
        }
    }
    Ok(())
}

fn write_socks_reply(stream: &mut TcpStream, status: u8) -> io::Result<()> {
    stream.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0])
}

fn proxy_bidirectional(client: TcpStream, upstream: TcpStream) -> io::Result<()> {
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let client_to_upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });

    let mut upstream_read = upstream;
    let mut client_write = client;
    let result = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = client_to_upstream.join();
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::os::unix::net::UnixStream;

    #[test]
    fn validate_domain_pattern_rejects_exact_url() {
        let err = validate_domain_pattern("https://pypi.org/simple/").expect_err("url rejected");
        assert!(err.contains("host pattern"), "unexpected error: {err}");
    }

    #[test]
    fn host_policy_matches_domain_patterns() {
        let policy = HostPolicy::new(&ManagedProxyConfig {
            allowed_domains: vec![
                "pypi.org".to_string(),
                "*.pythonhosted.org".to_string(),
                "**.r-project.org".to_string(),
            ],
            denied_domains: Vec::new(),
            allow_local_binding: false,
        })
        .expect("policy");

        assert!(policy.allows("pypi.org"));
        assert!(policy.allows("files.pythonhosted.org"));
        assert!(!policy.allows("pythonhosted.org"));
        assert!(policy.allows("r-project.org"));
        assert!(policy.allows("cran.r-project.org"));
        assert!(!policy.allows("evilr-project.org"));
    }

    #[test]
    fn host_policy_denied_domains_win() {
        let policy = HostPolicy::new(&ManagedProxyConfig {
            allowed_domains: vec!["**.example.com".to_string()],
            denied_domains: vec!["blocked.example.com".to_string()],
            allow_local_binding: false,
        })
        .expect("policy");

        assert!(policy.allows("api.example.com"));
        assert!(!policy.allows("blocked.example.com"));
    }

    #[test]
    fn host_policy_blocks_private_resolved_addresses_unless_local_binding_is_allowed() {
        let blocked = HostPolicy::new(&ManagedProxyConfig {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: false,
        })
        .expect("policy");
        let allowed = HostPolicy::new(&ManagedProxyConfig {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: true,
        })
        .expect("policy");

        let loopback = SocketAddr::from(([127, 0, 0, 1], 443));
        let private = SocketAddr::from(([10, 0, 0, 1], 443));
        assert!(!blocked.allows_socket_addr(loopback));
        assert!(allowed.allows_socket_addr(loopback));
        assert!(!blocked.allows_socket_addr(private));
        assert!(!allowed.allows_socket_addr(private));
    }

    #[test]
    fn non_public_ipv4_classifier_blocks_special_ranges() {
        for raw in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
        ] {
            let ip = raw.parse::<Ipv4Addr>().expect("IPv4 address");
            assert!(is_non_public_ipv4(ip), "{raw} should be non-public");
        }

        assert!(!is_non_public_ipv4(
            "8.8.8.8".parse::<Ipv4Addr>().expect("IPv4 address")
        ));
    }

    #[test]
    fn non_public_ipv6_classifier_unwraps_ipv4_mapped_addresses() {
        for raw in ["::ffff:127.0.0.1", "::ffff:100.64.0.1"] {
            let ip = raw.parse::<Ipv6Addr>().expect("IPv6 address");
            assert!(is_non_public_ipv6(ip), "{raw} should be non-public");
        }

        assert!(!is_non_public_ipv6(
            "::ffff:8.8.8.8".parse::<Ipv6Addr>().expect("IPv6 address")
        ));
    }

    #[test]
    fn loopback_classifier_unwraps_ipv4_mapped_addresses() {
        assert!(is_loopback_ip(IpAddr::V6(
            "::ffff:127.0.0.1"
                .parse::<Ipv6Addr>()
                .expect("IPv6 address")
        )));
        assert!(!is_loopback_ip(IpAddr::V6(
            "::ffff:10.0.0.1".parse::<Ipv6Addr>().expect("IPv6 address")
        )));
    }

    #[test]
    fn managed_proxy_apply_to_env_overrides_common_proxy_vars() {
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: false,
        })
        .expect("proxy");
        let mut env = HashMap::from([(
            "HTTP_PROXY".to_string(),
            "http://proxy.example:8080".to_string(),
        )]);

        proxy.apply_to_env(&mut env);

        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some(format!("http://{}", proxy.http_addr()).as_str())
        );
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some(format!("http://{}", proxy.http_addr()).as_str())
        );
        assert_eq!(
            env.get("ALL_PROXY").map(String::as_str),
            Some(format!("socks5h://{}", proxy.socks_addr()).as_str())
        );
    }

    #[test]
    fn managed_http_proxy_forwards_allowed_absolute_http_request() {
        let origin = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("origin");
        let origin_addr = origin.local_addr().expect("origin address");
        let origin_thread = thread::spawn(move || {
            let (mut stream, _) = origin.accept().expect("origin accept");
            let request = read_http_header(&mut stream).expect("request header");
            let request = String::from_utf8(request).expect("request utf8");
            assert!(
                request.starts_with("GET /packages HTTP/1.1\r\n"),
                "proxy should rewrite absolute-form target: {request}"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("origin response");
        });
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["127.0.0.1".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: true,
        })
        .expect("proxy");

        let mut client = TcpStream::connect(proxy.http_addr()).expect("connect proxy");
        client
            .write_all(
                format!(
                    "GET http://127.0.0.1:{}/packages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    origin_addr.port(),
                    origin_addr.port()
                )
                .as_bytes(),
            )
            .expect("proxy request");
        client
            .shutdown(Shutdown::Write)
            .expect("finish request body");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("proxy response");

        assert!(response.contains("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
        origin_thread.join().expect("origin thread");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unix_socket_relay_forwards_bytes_to_managed_proxy() {
        let origin = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("origin");
        let origin_addr = origin.local_addr().expect("origin address");
        let origin_thread = thread::spawn(move || {
            let (mut stream, _) = origin.accept().expect("origin accept");
            let request = read_http_header(&mut stream).expect("request header");
            let request = String::from_utf8(request).expect("request utf8");
            assert!(
                request.starts_with("GET /relay HTTP/1.1\r\n"),
                "proxy should receive traffic forwarded through the Unix relay: {request}"
            );
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nrelayed",
                )
                .expect("origin response");
        });
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["127.0.0.1".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: true,
        })
        .expect("proxy");
        let temp = tempfile::Builder::new()
            .prefix("mcp-repl-linux-relay-")
            .tempdir()
            .expect("tempdir");
        let mut env = HashMap::new();
        let bridge = proxy
            .apply_linux_bridge_to_env(&mut env, temp.path())
            .expect("Linux relay sockets");

        let mut client = UnixStream::connect(&bridge.http_socket).expect("connect HTTP relay");
        client
            .write_all(
                format!(
                    "GET http://127.0.0.1:{}/relay HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    origin_addr.port(),
                    origin_addr.port()
                )
                .as_bytes(),
            )
            .expect("relay request");
        client
            .shutdown(Shutdown::Write)
            .expect("finish relay request");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("relay response");

        assert!(response.contains("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("relayed"), "{response}");
        origin_thread.join().expect("origin thread");
    }

    #[test]
    fn managed_http_proxy_closes_plain_http_after_one_request() {
        let origin = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("origin");
        let origin_addr = origin.local_addr().expect("origin address");
        let origin_thread = thread::spawn(move || {
            let (mut stream, _) = origin.accept().expect("origin accept");
            let request = read_http_header(&mut stream).expect("request header");
            let request = String::from_utf8(request).expect("request utf8");
            assert!(
                request.starts_with("GET /packages HTTP/1.1\r\n"),
                "proxy should rewrite absolute-form target: {request}"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("origin response");
            if request.contains("\r\nConnection: close\r\n") {
                return;
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("origin read timeout");
            let _ = stream.read(&mut [0_u8; 1]);
        });
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["127.0.0.1".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: true,
        })
        .expect("proxy");

        let mut client = TcpStream::connect(proxy.http_addr()).expect("connect proxy");
        client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("client read timeout");
        client
            .write_all(
                format!(
                    "GET http://127.0.0.1:{}/packages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: keep-alive\r\n\r\n",
                    origin_addr.port(),
                    origin_addr.port()
                )
                .as_bytes(),
            )
            .expect("proxy request");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("proxy should close one-request HTTP connection");

        assert!(response.contains("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
        origin_thread.join().expect("origin thread");
    }

    #[test]
    fn managed_http_proxy_blocks_disallowed_host_without_dialing() {
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: false,
        })
        .expect("proxy");

        let mut client = TcpStream::connect(proxy.http_addr()).expect("connect proxy");
        client
            .write_all(
                b"GET http://not-allowed.invalid/packages HTTP/1.1\r\nHost: not-allowed.invalid\r\nConnection: close\r\n\r\n",
            )
            .expect("proxy request");
        client
            .shutdown(Shutdown::Write)
            .expect("finish request body");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("proxy response");

        assert!(response.contains("HTTP/1.1 403 Forbidden"), "{response}");
        assert!(
            response.contains("blocked by network allowlist"),
            "{response}"
        );
    }

    #[test]
    fn managed_http_proxy_rejects_host_header_mismatch() {
        let proxy = ManagedNetworkProxy::start(ManagedProxyConfig {
            allowed_domains: vec!["127.0.0.1".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: true,
        })
        .expect("proxy");

        let mut client = TcpStream::connect(proxy.http_addr()).expect("connect proxy");
        client
            .write_all(
                b"GET http://127.0.0.1:9/packages HTTP/1.1\r\nHost: not-allowed.invalid\r\nConnection: close\r\n\r\n",
            )
            .expect("proxy request");
        client
            .shutdown(Shutdown::Write)
            .expect("finish request body");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("proxy response");

        assert!(response.contains("HTTP/1.1 403 Forbidden"), "{response}");
        assert!(response.contains("Host header"), "{response}");
    }
}
