//! The gate in-process: real TCP on 127.0.0.1, no sandbox.

use std::sync::Arc;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use super::*;

/// An echo server that answers its first line, standing in for a destination.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let _ = s.write_all(b"echo:").await;
                let _ = s.write_all(&buf[..n]).await;
            });
        }
    });
    port
}

/// A decider: allows destinations named in `allow` (as `host:port`), literal when `literal`; denies the rest.
fn decide(allow: &'static [&'static str], literal: bool) -> Decide {
    Arc::new(move |_user, to: Destination| {
        let verdict = match allow.contains(&to.to_string().as_str()) {
            true => Verdict::Allow { literal },
            false => Verdict::Deny {
                reason: format!("Piton's sandbox refused {to}: no connect entry allows it"),
            },
        };
        Box::pin(async move { verdict })
    })
}

async fn send(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn basic(user: &str, credential: &str) -> String {
    STANDARD.encode(format!("{user}:{credential}"))
}

/// No credential, or another user's: `407`. With its own: decided and tunneled. A refusal answers `403` with its
/// reason as the status phrase and is reported.
#[tokio::test]
async fn authenticates_decides_and_tunnels() {
    let echo = echo_server().await;
    let allowed: &'static str = Box::leak(format!("127.0.0.1:{echo}").into_boxed_str());
    let allow: &'static [&'static str] = Box::leak(vec![allowed].into_boxed_slice());
    let (tx, mut refused) = tokio::sync::mpsc::unbounded_channel();
    let gate = Gate::start(decide(allow, true), Some(Route::Direct), tx).unwrap();
    let (user, credential) = gate.issue("pi");
    let (_, other) = gate.issue("s-1");

    let no_auth = send(gate.port, &format!("CONNECT {allowed} HTTP/1.1\r\n\r\n")).await;
    assert!(no_auth.starts_with("HTTP/1.1 407"), "{no_auth}");
    let wrong = send(
        gate.port,
        &format!(
            "CONNECT {allowed} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic(&user, &other)
        ),
    )
    .await;
    assert!(
        wrong.starts_with("HTTP/1.1 407"),
        "another sandbox's credential: {wrong}"
    );

    let auth = basic(&user, &credential);
    let tunneled = send(
        gate.port,
        &format!("CONNECT {allowed} HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\nhello"),
    )
    .await;
    assert!(tunneled.starts_with("HTTP/1.1 200"), "{tunneled}");
    assert!(
        tunneled.ends_with("echo:hello"),
        "bytes after the head reach the destination: {tunneled}"
    );

    let denied = send(
        gate.port,
        &format!("CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(
        denied.starts_with("HTTP/1.1 403 Forbidden: Piton's sandbox refused example.com:443"),
        "{denied}"
    );
    let r = refused.recv().await.unwrap();
    assert_eq!(
        (r.user, r.to.to_string()),
        (User("pi".into()), "example.com:443".to_string())
    );

    // A forgotten sandbox's credential no longer works.
    gate.forget("pi");
    let gone = send(
        gate.port,
        &format!("CONNECT {allowed} HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(gone.starts_with("HTTP/1.1 407"), "{gone}");
}

/// Plain HTTP in absolute form is forwarded in origin form, without proxy headers.
#[tokio::test]
async fn forwards_plain_http() {
    let echo = echo_server().await;
    let allowed: &'static str = Box::leak(format!("127.0.0.1:{echo}").into_boxed_str());
    let allow: &'static [&'static str] = Box::leak(vec![allowed].into_boxed_slice());
    let (tx, _refused) = tokio::sync::mpsc::unbounded_channel();
    let gate = Gate::start(decide(allow, true), Some(Route::Direct), tx).unwrap();
    let (user, credential) = gate.issue("pi");
    let out = send(
        gate.port,
        &format!(
            "GET http://{allowed}/x?y=1 HTTP/1.1\r\nHost: {allowed}\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic(&user, &credential)
        ),
    )
    .await;
    assert!(out.starts_with("echo:GET /x?y=1 HTTP/1.1\r\n"), "{out}");
    assert!(!out.contains("Proxy-Authorization"), "{out}");
    assert!(out.contains("Connection: close"), "{out}");
}

/// An allowed name that resolves to this Mac isn't dialed unless an IP entry listed the address.
#[tokio::test]
async fn names_resolving_to_this_mac_are_refused() {
    let echo = echo_server().await;
    let name: &'static str = Box::leak(format!("localhost:{echo}").into_boxed_str());
    let allow: &'static [&'static str] = Box::leak(vec![name].into_boxed_slice());
    let (tx, mut refused) = tokio::sync::mpsc::unbounded_channel();
    let gate = Gate::start(decide(allow, false), Some(Route::Direct), tx).unwrap();
    let (user, credential) = gate.issue("pi");
    let out = send(
        gate.port,
        &format!(
            "CONNECT {name} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic(&user, &credential)
        ),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    assert!(out.contains("a loopback address"), "{out}");
    assert!(refused.recv().await.unwrap().reason.contains("loopback"));
}

/// Through an upstream proxy, every tunnel is a CONNECT to it with its credentials; until the upstream is known, a
/// connection waits for it.
#[tokio::test]
async fn chains_to_an_upstream_proxy() {
    let echo = echo_server().await;
    // A fake upstream: checks its credentials, then tunnels to the echo server whatever the target.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    tokio::spawn({
        let seen = seen.clone();
        async move {
            loop {
                let Ok((mut s, _)) = upstream.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if s.read(&mut b).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(b[0]);
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    crate::session::lock(&seen).push(text.clone());
                    if !text.contains(&format!("Basic {}", STANDARD.encode("fnox:pw"))) {
                        let _ = s.write_all(b"HTTP/1.1 407 No\r\n\r\n").await;
                        return;
                    }
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
                    let mut d = TcpStream::connect(("127.0.0.1", echo)).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut s, &mut d).await;
                });
            }
        }
    });
    let allow: &'static [&'static str] = &["github.com:22"];
    let (tx, _refused) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(Gate::start(decide(allow, false), None, tx).unwrap());
    let (user, credential) = gate.issue("pi");
    let auth = basic(&user, &credential);
    let request = format!("CONNECT github.com:22 HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\nping");
    let pending = tokio::spawn({
        let port = gate.port;
        async move { send(port, &request).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.set_route(parse_upstream(&format!("http://fnox:pw@127.0.0.1:{upstream_port}")).unwrap());
    let out = pending.await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("echo:ping"), "{out}");
    assert!(crate::session::lock(&seen)[0].starts_with("CONNECT github.com:22 HTTP/1.1"));
}

/// The helper's proxy variables win over the environment's, upper and lower case, with Node's switch and the ssh
/// wrapper first on PATH (once).
#[test]
fn the_proxy_environment() {
    let mut env: crate::launch::Env = [
        ("HTTPS_PROXY".to_string(), "http://fnox:x@127.0.0.1:1".to_string()),
        ("PATH".to_string(), "/w/net:/usr/bin".to_string()),
        ("SSL_CERT_FILE".to_string(), "/t/fnox.pem".to_string()),
    ]
    .into();
    let url = proxy_url(7, "pi", "c");
    set_proxy_env(&mut env, &url, Some(std::path::Path::new("/w/net")));
    for name in PROXY_VARS {
        assert_eq!(env[name], "http://pi:c@127.0.0.1:7", "{name}");
    }
    assert_eq!(env["no_proxy"], "localhost,127.0.0.1,::1");
    assert_eq!(env["NODE_USE_ENV_PROXY"], "1");
    assert_eq!(env["PATH"], "/w/net:/usr/bin");
    assert_eq!(env["SSL_CERT_FILE"], "/t/fnox.pem", "fnox's CA stays");
}

#[test]
fn parses_heads_and_upstreams() {
    let head = parse_head(b"CONNECT GitHub.com:22 HTTP/1.1\r\nProxy-Authorization: Basic cGk6Yw==\r\n\r\n").unwrap();
    assert_eq!(head.auth.as_deref(), Some("pi:c"));
    assert_eq!(
        head.request,
        Request::Connect(Destination::parse("github.com:22").unwrap())
    );
    let http = parse_head(b"GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
    assert!(matches!(&http.request, Request::Http { to, .. } if to.to_string() == "example.com:80"));
    for bad in [
        &b"GET /x HTTP/1.1\r\n\r\n"[..],
        b"CONNECT example.com HTTP/1.1\r\n\r\n",
        b"nonsense\r\n\r\n",
    ] {
        assert!(parse_head(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }
    assert_eq!(
        parse_upstream("http://fnox:abc@127.0.0.1:50862"),
        Some(Route::Upstream {
            address: "127.0.0.1:50862".parse().unwrap(),
            auth: Some("fnox:abc".into())
        })
    );
    assert_eq!(parse_upstream("https://x"), None);
    assert_eq!(status_phrase("a\nb ü"), "Forbidden: a?b ?");
}
