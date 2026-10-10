//! Getting out of a native host: direct connections, or through the HTTP(S) proxy the
//! environment names (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) — sandboxes often
//! allow nothing else. TLS uses the system's roots (`SSL_CERT_FILE` included), so proxies that
//! intercept TLS with their own CA work too.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    /// `Basic …` for Proxy-Authorization
    pub auth: Option<String>,
}

impl Proxy {
    /// From `http://[user:pass@]host[:port]`.
    pub fn parse(url: &str) -> Option<Proxy> {
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let rest = rest.split('/').next()?;
        let (auth, hostport) = match rest.rsplit_once('@') {
            Some((a, h)) => (
                Some(format!(
                    "Basic {}",
                    crate::dev::web::base64_encode(a.as_bytes())
                )),
                h,
            ),
            None => (None, rest),
        };
        let (host, port) = split_host_port(hostport, 80)?;
        Some(Proxy { host, port, auth })
    }
}

fn split_host_port(s: &str, default: u16) -> Option<(String, u16)> {
    if let Some(r) = s.strip_prefix('[') {
        let (h, rest) = r.split_once(']')?;
        let port = rest
            .strip_prefix(':')
            .map_or(Some(default), |p| p.parse().ok())?;
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default)),
    }
}

/// How the host reaches the outside world on the guest's behalf.
#[derive(Clone, Debug, Default)]
pub struct Egress {
    /// For https://, wss:// and other TCP (tunnelled with CONNECT)
    pub https_proxy: Option<Proxy>,
    /// For http:// and ws://
    pub http_proxy: Option<Proxy>,
    pub no_proxy: Vec<String>,
    /// Allow connections to loopback, private and link-local addresses (off: the guest can't
    /// reach the host's own services or its network).
    pub allow_private: bool,
}

fn env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
}

impl Egress {
    pub fn from_env() -> Egress {
        let all = env(&["ALL_PROXY", "all_proxy"]);
        Egress {
            https_proxy: env(&["HTTPS_PROXY", "https_proxy"])
                .or(all.clone())
                .and_then(|u| Proxy::parse(&u)),
            http_proxy: env(&["HTTP_PROXY", "http_proxy"])
                .or(all)
                .and_then(|u| Proxy::parse(&u)),
            no_proxy: env(&["NO_PROXY", "no_proxy"])
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            allow_private: false,
        }
    }

    fn bypass(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.no_proxy.iter().any(|p| {
            p == "*" || {
                let p = p.trim_start_matches("*.").trim_start_matches('.');
                host == p || host.ends_with(&format!(".{p}"))
            }
        })
    }

    fn proxy_for(&self, host: &str, tunnel: bool) -> Option<&Proxy> {
        if self.bypass(host) {
            return None;
        }
        if tunnel {
            self.https_proxy.as_ref()
        } else {
            self.http_proxy.as_ref()
        }
    }

    fn direct(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
        if !self.allow_private {
            if let Some(a) = addrs.iter().find(|a| private(a.ip())) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{host} is a private address ({}): not reachable from the guest",
                        a.ip()
                    ),
                ));
            }
        }
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address"));
        for a in addrs {
            match TcpStream::connect_timeout(&a, TIMEOUT) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                },
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// A TCP stream to host:port: direct, or tunnelled through the proxy with CONNECT.
    pub fn connect(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let Some(p) = self.proxy_for(host, true) else {
            return self.direct(host, port);
        };
        let mut s = TcpStream::connect((p.host.as_str(), p.port))?;
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(TIMEOUT))?;
        let target =
            if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
        if let Some(a) = &p.auth {
            req.push_str(&format!("Proxy-Authorization: {a}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())?;
        // Read the proxy's answer byte by byte: what follows it belongs to the tunnel
        let mut head = Vec::new();
        let mut b = [0u8];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b)? == 0 || head.len() > 16384 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the proxy closed the tunnel",
                ));
            }
            head.push(b[0]);
        }
        let status = String::from_utf8_lossy(&head);
        let code = status.split_whitespace().nth(1).unwrap_or("");
        if code != "200" {
            let line = status.lines().next().unwrap_or("").to_string();
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("proxy: {line}"),
            ));
        }
        s.set_read_timeout(None)?;
        Ok(s)
    }
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.octets()[0] == 100 && (v.octets()[1] & 0xc0) == 64
        },
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
        },
    }
}

/// A stream that may be TLS.
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(b),
            Conn::Tls(s) => match s.read(b) {
                // Servers that close without close_notify: an end, not an error
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                r => r,
            },
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(b),
            Conn::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

impl Conn {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => s.get_ref(),
        }
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for c in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(c);
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// The parts of a URL a request needs.
pub struct Target {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Option<Target> {
        let (scheme, rest) = url.split_once("://")?;
        let secure = match scheme.to_ascii_lowercase().as_str() {
            "https" | "wss" => true,
            "http" | "ws" => false,
            _ => return None,
        };
        let (hostport, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let path = if path.starts_with('?') { format!("/{path}") } else { path };
        let (host, port) = split_host_port(hostport, if secure { 443 } else { 80 })?;
        Some(Target {
            secure,
            host,
            port,
            path,
        })
    }

    fn authority(&self) -> String {
        let host =
            if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.secure { 443 } else { 80 } {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl Egress {
    /// Open the stream for a request to `t`, and the request target to put in it (absolute
    /// for plain HTTP through a proxy).
    pub fn open(&self, t: &Target) -> io::Result<(Conn, String)> {
        if !t.secure {
            if let Some(p) = self.proxy_for(&t.host, false) {
                let s = TcpStream::connect((p.host.as_str(), p.port))?;
                let _ = s.set_nodelay(true);
                return Ok((
                    Conn::Plain(s),
                    format!("http://{}{}", t.authority(), t.path),
                ));
            }
            return Ok((Conn::Plain(self.connect(&t.host, t.port)?), t.path.clone()));
        }
        let tcp = self.connect(&t.host, t.port)?;
        let name = rustls::pki_types::ServerName::try_from(t.host.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let conn = rustls::ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        Ok((
            Conn::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))),
            t.path.clone(),
        ))
    }

    fn proxy_auth(&self, t: &Target) -> Option<String> {
        if t.secure {
            return None;
        }
        self.proxy_for(&t.host, false).and_then(|p| p.auth.clone())
    }
}

/// An HTTP/1.1 response being read.
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

fn read_head(r: &mut BufReader<Conn>) -> io::Result<(u16, String, Vec<(String, String)>)> {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no response"));
        }
        let mut parts = line.trim_end().splitn(3, ' ');
        let _version = parts.next();
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::other("bad status line"))?;
        let reason = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h)? == 0 || h.trim_end().is_empty() {
                break;
            }
            if let Some((k, v)) = h.trim_end().split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        // Interim answers (100 Continue, 103 Early Hints): the real one follows
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        return Ok((status, reason, headers));
    }
}

/// `Transfer-Encoding: chunked`, decoded.
struct Chunked {
    r: BufReader<Conn>,
    left: usize,
    done: bool,
}

impl Read for Chunked {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            if line.trim().is_empty() {
                line.clear();
                self.r.read_line(&mut line)?;
            }
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| io::Error::other("bad chunk size"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let max = out.len().min(self.left);
        let n = self.r.read(&mut out[..max])?;
        if n == 0 {
            self.done = true;
        }
        self.left -= n;
        if self.left == 0 {
            let mut crlf = [0u8; 2];
            let _ = self.r.read_exact(&mut crlf);
        }
        Ok(n)
    }
}

const SKIP_REQUEST: &[&str] = &[
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "proxy-connection",
];

impl Egress {
    /// Make an HTTP request (the guest's, as the web relay saw it).
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> io::Result<Response> {
        let t = Target::parse(url)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad URL {url}")))?;
        let (mut conn, target) = self.open(&t)?;
        let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {}\r\n", t.authority());
        for (k, v) in headers {
            if !SKIP_REQUEST.contains(&k.to_ascii_lowercase().as_str()) {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        if !body.is_empty()
            || !matches!(
                method.to_ascii_uppercase().as_str(),
                "GET" | "HEAD" | "DELETE" | "OPTIONS"
            )
        {
            req.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        if let Some(a) = self.proxy_auth(&t) {
            req.push_str(&format!("Proxy-Authorization: {a}\r\n"));
        }
        req.push_str("connection: close\r\n\r\n");
        conn.write_all(req.as_bytes())?;
        conn.write_all(body)?;
        conn.flush()?;
        let mut r = BufReader::with_capacity(64 * 1024, conn);
        let (status, reason, headers) = read_head(&mut r)?;
        let get = |k: &str| {
            headers
                .iter()
                .find(|(h, _)| h.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304;
        let body: Box<dyn Read + Send> = if no_body {
            Box::new(io::empty())
        } else if get("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            Box::new(Chunked {
                r,
                left: 0,
                done: false,
            })
        } else if let Some(n) = get("content-length").and_then(|v| v.parse::<u64>().ok()) {
            Box::new(r.take(n))
        } else {
            Box::new(r)
        };
        Ok(Response {
            status,
            reason,
            headers,
            body,
        })
    }

    /// Open a WebSocket: the server's chosen subprotocol, and the stream (frames from here on).
    pub fn websocket(
        &self,
        url: &str,
        protocols: &[String],
        key: &str,
    ) -> io::Result<(String, BufReader<Conn>)> {
        let t = Target::parse(url)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad URL {url}")))?;
        // A WebSocket is a long-lived stream: always a tunnel (or direct), never a forwarding proxy
        let tcp = self.connect(&t.host, t.port)?;
        let mut conn = if t.secure {
            let name = rustls::pki_types::ServerName::try_from(t.host.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let c = rustls::ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
            Conn::Tls(Box::new(rustls::StreamOwned::new(c, tcp)))
        } else {
            Conn::Plain(tcp)
        };
        let mut req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n",
            t.path,
            t.authority()
        );
        if !protocols.is_empty() {
            req.push_str(&format!(
                "Sec-WebSocket-Protocol: {}\r\n",
                protocols.join(", ")
            ));
        }
        req.push_str("\r\n");
        conn.write_all(req.as_bytes())?;
        conn.flush()?;
        let mut r = BufReader::with_capacity(64 * 1024, conn);
        let (status, reason, headers) = read_head(&mut r)?;
        if status != 101 {
            return Err(io::Error::other(format!(
                "the server answered {status} {reason}"
            )));
        }
        let protocol = headers
            .iter()
            .find(|(h, _)| h.eq_ignore_ascii_case("sec-websocket-protocol"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Ok((protocol, r))
    }
}

/// A WebSocket frame from a server (unmasked): (fin, opcode, payload).
pub fn read_frame(r: &mut impl Read) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h)?;
    let mut len = (h[1] & 127) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    let mask = if h[1] & 128 != 0 {
        let mut m = [0u8; 4];
        r.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    if len > 64 << 20 {
        return Err(io::Error::other("WebSocket frame too large"));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if let Some(m) = mask {
        for (i, x) in payload.iter_mut().enumerate() {
            *x ^= m[i & 3];
        }
    }
    Ok((h[0] & 0x80 != 0, h[0] & 15, payload))
}

/// A WebSocket frame to a server (clients must mask).
pub fn client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let n = payload.len();
    let mut f = vec![0x80 | opcode];
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n < 65536 {
        f.extend_from_slice(&[0x80 | 126, (n >> 8) as u8, n as u8]);
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    f
}
