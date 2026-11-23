//! The gate: the HTTP proxy a sandbox with a filtered network reaches named hosts through. The sandbox's only route
//! to a named host is the gate's port on 127.0.0.1; the gate checks each connection's destination and tunnels it,
//! any port, not only HTTPS.
//!
//! - Each sandbox the daemon starts for a session (pi's, each bash call's) gets its own proxy user and credential, so a
//!   connection names the sandbox it came from and can't borrow another's network. Anything without a known user and
//!   its credential gets `407`.
//! - The session decides (its [`Decide`]): the sandbox's `connect` entries, pi's trusted hosts, the session's answers,
//!   `silence`, else the user is asked while the connection waits.
//! - An allowed name is resolved once, and an address that reaches this Mac or its link isn't dialed
//!   ([`addresses`]), unless an IP entry lists it.
//! - Through an upstream proxy (fnox's), every allowed tunnel is a `CONNECT` to it; the upstream dials.
//!
//! `CONNECT host:port` and plain HTTP in absolute form (`GET http://host/path`) are served; one request per connection
//! for plain HTTP (`Connection: close`).

pub mod addresses;
pub mod ssh;

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

use crate::launch::sandbox::Destination;

/// The longest request head the gate reads.
const MAX_HEAD: usize = 16 * 1024;
/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a connect to a destination may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a connection waits for the upstream proxy to be reported before it's refused.
const UPSTREAM_WAIT: Duration = Duration::from_secs(10);

/// What the session decides about one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Tunnel it. `literal`: an IP entry listed this exact address, so the resolved-address check doesn't apply.
    Allow { literal: bool },
    /// Refuse it, with why (the `403`'s reason: the host, that Piton's sandbox refused it, what would allow it).
    Deny { reason: String },
}

/// Who asks: the sandbox a connection's proxy user names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct User(pub String);

/// The session's decision for a connection: called once per connection, may wait for the user.
pub type Decide = Arc<dyn Fn(User, Destination) -> BoxFuture<'static, Verdict> + Send + Sync>;

/// A refusal the gate made, for the session's records (`network-outbound host:port`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub user: User,
    pub to: Destination,
    pub reason: String,
}

/// Where allowed connections go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The gate dials.
    Direct,
    /// Through an upstream HTTP proxy (`http://user:pass@127.0.0.1:port`): every tunnel is a `CONNECT` to it.
    Upstream { address: SocketAddr, auth: Option<String> },
}

/// A running gate. Dropping it stops listening; open tunnels finish on their own.
pub struct Gate {
    pub port: u16,
    users: Arc<Mutex<HashMap<String, User>>>,
    route: Arc<RouteState>,
    task: tokio::task::JoinHandle<()>,
}

/// The route, which may be learned after the gate starts (fnox's URL is known only once fnox runs).
struct RouteState {
    route: Mutex<Option<Route>>,
    set: tokio::sync::Notify,
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Gate {
    /// Listens on `127.0.0.1` at a port the system picks, inside the current Tokio runtime. `route` is `None` until
    /// [`Gate::set_route`] when an upstream is expected; connections wait for it up to [`UPSTREAM_WAIT`].
    pub fn start(
        decide: Decide,
        route: Option<Route>,
        refused: tokio::sync::mpsc::UnboundedSender<Refused>,
    ) -> std::io::Result<Self> {
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;
        let port = listener.local_addr()?.port();
        let users: Arc<Mutex<HashMap<String, User>>> = Arc::default();
        let route = Arc::new(RouteState {
            route: Mutex::new(route),
            set: tokio::sync::Notify::new(),
        });
        let task = tokio::spawn({
            let (users, route) = (users.clone(), route.clone());
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let ctx = Connection {
                        users: users.clone(),
                        decide: decide.clone(),
                        route: route.clone(),
                        refused: refused.clone(),
                    };
                    tokio::spawn(async move { ctx.serve(stream).await });
                }
            }
        });
        Ok(Self {
            port,
            users,
            route,
            task,
        })
    }

    /// A new proxy user for one sandbox: its name and a fresh credential, `user:credential` in the proxy URL.
    pub fn issue(&self, name: &str) -> (String, String) {
        let credential = crate::agent::random_hex(32);
        let key = format!("{name}:{credential}");
        crate::session::lock(&self.users).insert(key, User(name.to_string()));
        (name.to_string(), credential)
    }

    /// Forgets a sandbox's credential (its call ended).
    pub fn forget(&self, name: &str) {
        crate::session::lock(&self.users).retain(|_, user| user.0 != name);
    }

    /// Sets where allowed connections go, waking those waiting for it.
    pub fn set_route(&self, route: Route) {
        *crate::session::lock(&self.route.route) = Some(route);
        self.route.set.notify_waiters();
    }
}

/// The proxy URL a sandbox's processes use.
pub fn proxy_url(port: u16, user: &str, credential: &str) -> String {
    format!("http://{user}:{credential}@127.0.0.1:{port}")
}

/// The proxy variables, set over whatever the environment had: upper and lower case (curl reads only the lowercase
/// `http_proxy` for `http://`), `NO_PROXY` for this Mac, and `NODE_USE_ENV_PROXY=1` (Node's built-in `fetch`, `http`,
/// and `https` read the proxy variables only with it).
pub const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Sets a sandbox's proxy variables to its gate, and puts `ssh_dir` (the `ssh` wrapper) first on `PATH`.
pub fn set_proxy_env(env: &mut crate::launch::Env, url: &str, ssh_dir: Option<&std::path::Path>) {
    for name in PROXY_VARS {
        env.insert(name.into(), url.into());
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.insert(name.into(), "localhost,127.0.0.1,::1".into());
    }
    env.insert("NODE_USE_ENV_PROXY".into(), "1".into());
    if let Some(dir) = ssh_dir {
        let path = env.get("PATH").cloned().unwrap_or_default();
        let dir = dir.to_string_lossy();
        let rest = path.split(':').filter(|p| !p.is_empty() && *p != dir);
        env.insert(
            "PATH".into(),
            std::iter::once(dir.as_ref()).chain(rest).collect::<Vec<_>>().join(":"),
        );
    }
}

struct Connection {
    users: Arc<Mutex<HashMap<String, User>>>,
    decide: Decide,
    route: Arc<RouteState>,
    refused: tokio::sync::mpsc::UnboundedSender<Refused>,
}

/// A request the gate serves.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    /// `CONNECT host:port`.
    Connect(Destination),
    /// Plain HTTP in absolute form: the destination, and the head to forward (in origin form, `Connection: close`,
    /// without proxy headers).
    Http { to: Destination, head: Vec<u8> },
}

/// A parsed request head.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    request: Request,
    /// `user:credential` from `Proxy-Authorization: Basic`.
    auth: Option<String>,
}

impl Connection {
    async fn serve(self, stream: TcpStream) {
        let mut reader = BufReader::new(stream);
        let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut reader)).await {
            Ok(Ok(head)) => head,
            Ok(Err(_)) | Err(_) => {
                let _ = respond(
                    reader.get_mut(),
                    400,
                    "Bad Request",
                    "Piton's gate: not a proxy request",
                )
                .await;
                return;
            }
        };
        let head = match parse_head(&head) {
            Ok(head) => head,
            Err(why) => {
                let _ = respond(reader.get_mut(), 400, "Bad Request", &why).await;
                return;
            }
        };
        let user = head
            .auth
            .as_ref()
            .and_then(|auth| crate::session::lock(&self.users).get(auth).cloned());
        let Some(user) = user else {
            let _ = reader
                .get_mut()
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"piton\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        };
        let to = match &head.request {
            Request::Connect(to) | Request::Http { to, .. } => to.clone(),
        };
        let verdict = (self.decide)(user.clone(), to.clone()).await;
        let literal = match verdict {
            Verdict::Allow { literal } => literal,
            Verdict::Deny { reason } => {
                return self.refuse(reader.get_mut(), user, to, reason).await;
            }
        };
        let route = match self.wait_route().await {
            Some(route) => route,
            None => {
                let reason = "Piton's gate: fnox's proxy hasn't started; try again".to_string();
                let _ = respond(reader.get_mut(), 502, "Bad Gateway", &reason).await;
                return;
            }
        };
        let upstream = match route {
            Route::Direct => match dial(&to, literal).await {
                Ok(stream) => stream,
                Err(Dial::Local(why)) => {
                    let reason = format!("{to} resolves to {why}, which Piton's sandbox doesn't reach by name");
                    return self.refuse(reader.get_mut(), user, to, reason).await;
                }
                Err(Dial::Failed(e)) => {
                    let _ = respond(reader.get_mut(), 502, "Bad Gateway", &format!("can't reach {to}: {e}")).await;
                    return;
                }
            },
            Route::Upstream { address, auth } => match via_upstream(address, auth.as_deref(), &to).await {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = respond(reader.get_mut(), 502, "Bad Gateway", &format!("can't reach {to}: {e}")).await;
                    return;
                }
            },
        };
        let mut upstream = upstream;
        // Anything the client sent after its head is already buffered: pass it on.
        let buffered = reader.buffer().to_vec();
        let mut client = reader.into_inner();
        match head.request {
            Request::Connect(_) => {
                if client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Request::Http { head: forwarded, .. } => {
                if upstream.write_all(&forwarded).await.is_err() {
                    return;
                }
            }
        }
        if !buffered.is_empty() && upstream.write_all(&buffered).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }

    async fn refuse(&self, client: &mut TcpStream, user: User, to: Destination, reason: String) {
        let _ = respond(client, 403, &status_phrase(&reason), &reason).await;
        let _ = self.refused.send(Refused { user, to, reason });
    }

    async fn wait_route(&self) -> Option<Route> {
        let deadline = tokio::time::Instant::now() + UPSTREAM_WAIT;
        loop {
            let notified = self.route.set.notified();
            if let Some(route) = crate::session::lock(&self.route.route).clone() {
                return Some(route);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }
}

/// A reason as an HTTP status phrase: printable ASCII only, and short (clients print it).
fn status_phrase(reason: &str) -> String {
    let phrase: String = reason
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .take(200)
        .collect();
    format!("Forbidden: {phrase}")
}

async fn respond(stream: &mut TcpStream, code: u16, phrase: &str, body: &str) -> std::io::Result<()> {
    let body = format!("{body}\n");
    let text = format!(
        "HTTP/1.1 {code} {phrase}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Proxy-Error: \
         blocked-by-piton\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).await
}

/// Reads up to the blank line ending a request head.
async fn read_head(reader: &mut BufReader<TcpStream>) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let n = reader.read_until(b'\n', &mut head).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if head.len() > MAX_HEAD {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let line = &head[before..];
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

/// Parses a request head: `CONNECT host:port`, or an absolute-form `http://` request.
fn parse_head(head: &[u8]) -> Result<Head, String> {
    let text = std::str::from_utf8(head).map_err(|_| "the request head isn't UTF-8".to_string())?;
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) if v.starts_with("HTTP/1.") => (m, t, v),
        _ => return Err(format!("not an HTTP/1 request: {request_line:?}")),
    };
    let mut auth = None;
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':').ok_or_else(|| format!("not a header: {line:?}"))?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "proxy-authorization" => {
                auth = value
                    .strip_prefix("Basic ")
                    .or_else(|| value.strip_prefix("basic "))
                    .and_then(|b64| STANDARD.decode(b64.trim()).ok())
                    .and_then(|bytes| String::from_utf8(bytes).ok());
            }
            "proxy-connection" | "connection" | "keep-alive" => {}
            _ => headers.push((name.to_string(), value.to_string())),
        }
    }
    let request = match method {
        "CONNECT" => Request::Connect(
            Destination::parse(target).ok_or_else(|| format!("CONNECT needs host:port, not {target:?}"))?,
        ),
        _ => {
            let rest = target
                .strip_prefix("http://")
                .ok_or_else(|| "Piton's gate takes CONNECT, or plain HTTP with an http:// URL".to_string())?;
            let (authority, path) = match rest.find(['/', '?']) {
                Some(at) => (&rest[..at], &rest[at..]),
                None => (rest, "/"),
            };
            let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
            let with_port = match authority.strip_prefix('[') {
                Some(_) if authority.contains("]:") => authority.to_string(),
                Some(_) => format!("{authority}:80"),
                None if authority.contains(':') => authority.to_string(),
                None => format!("{authority}:80"),
            };
            let to = Destination::parse(&with_port).ok_or_else(|| format!("not a host: {authority:?}"))?;
            let path = match path.starts_with('/') {
                true => path.to_string(),
                false => format!("/{path}"),
            };
            let mut forwarded = format!("{method} {path} {version}\r\n");
            for (name, value) in &headers {
                forwarded.push_str(&format!("{name}: {value}\r\n"));
            }
            forwarded.push_str("Connection: close\r\n\r\n");
            Request::Http {
                to,
                head: forwarded.into_bytes(),
            }
        }
    };
    Ok(Head { request, auth })
}

enum Dial {
    /// Every address the name resolved to reaches this Mac or its link.
    Local(addresses::Nearby),
    Failed(std::io::Error),
}

/// Connects to `to`, checking each resolved address unless `literal`.
async fn dial(to: &Destination, literal: bool) -> Result<TcpStream, Dial> {
    let resolved: Vec<SocketAddr> =
        tokio::time::timeout(DIAL_TIMEOUT, tokio::net::lookup_host((to.host.as_str(), to.port)))
            .await
            .map_err(|_| Dial::Failed(std::io::ErrorKind::TimedOut.into()))?
            .map_err(Dial::Failed)?
            .collect();
    let own: Vec<IpAddr> = match literal {
        true => Vec::new(),
        false => addresses::own(),
    };
    let mut first_local = None;
    let mut last_error = None;
    for address in resolved {
        if !literal && let Some(why) = addresses::local(address.ip(), &own) {
            first_local.get_or_insert(why);
            continue;
        }
        match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_error = Some(e),
            Err(_) => last_error = Some(std::io::ErrorKind::TimedOut.into()),
        }
    }
    match (last_error, first_local) {
        (Some(e), _) => Err(Dial::Failed(e)),
        (None, Some(why)) => Err(Dial::Local(why)),
        (None, None) => Err(Dial::Failed(std::io::ErrorKind::NotFound.into())),
    }
}

/// A tunnel to `to` through an upstream HTTP proxy.
async fn via_upstream(address: SocketAddr, auth: Option<&str>, to: &Destination) -> std::io::Result<TcpStream> {
    let mut stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    let mut request = format!("CONNECT {to} HTTP/1.1\r\nHost: {to}\r\n");
    if let Some(auth) = auth {
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", STANDARD.encode(auth)));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    // Read the upstream's response head byte by byte, so nothing of the tunnel is consumed.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > MAX_HEAD {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut byte = [0u8; 1];
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let first = status.lines().next().unwrap_or_default();
    match first.split(' ').nth(1) {
        Some("200") => Ok(stream),
        _ => Err(std::io::Error::other(format!("the upstream proxy answered {first:?}"))),
    }
}

/// An upstream proxy's address and credentials from its URL (`http://user:pass@127.0.0.1:port`).
pub fn parse_upstream(url: &str) -> Option<Route> {
    let rest = url.strip_prefix("http://")?.trim_end_matches('/');
    let (auth, host) = match rest.rsplit_once('@') {
        Some((auth, host)) => (Some(auth.to_string()), host),
        None => (None, rest),
    };
    let address: SocketAddr = host.parse().ok()?;
    Some(Route::Upstream { address, auth })
}

#[cfg(test)]
mod tests;
