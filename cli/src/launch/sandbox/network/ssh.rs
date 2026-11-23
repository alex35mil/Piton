//! SSH through the gate. A sandbox with a filtered network finds an `ssh` first on its `PATH`: a link to this binary in
//! the daemon's state folder (`<state>/net/bin/ssh`), which the sandbox may read but never write. Invoked as `ssh`, this
//! binary runs the real `ssh` (the next one on `PATH`, else `/usr/bin/ssh`) with a `ProxyCommand` of
//! `piton gate-connect %h %p`, which opens a `CONNECT` tunnel through the gate in `HTTPS_PROXY`. Git over SSH, `ssh`,
//! `scp`, and `rsync` reach listed hosts that way; the gate asks about the rest.
//!
//! The command line's `-o ProxyCommand` wins over the user's `ProxyCommand` or `ProxyJump` in `~/.ssh/config` (ssh
//! takes the first value it gets): a jump host is reached through the gate only as the destination.

use std::{
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};

/// The wrapper's folder in the daemon's state folder.
pub fn folder(state_dir: &Path) -> PathBuf {
    state_dir.join("net/bin")
}

/// Makes `<state>/net/bin/ssh` a link to this binary, replacing a stale one; `None` when it can't.
pub fn ensure(state_dir: &Path) -> Option<PathBuf> {
    let dir = folder(state_dir);
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join("ssh");
    if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&exe, &link).ok()?;
    }
    Some(dir)
}

/// Whether this binary was invoked as the `ssh` wrapper.
pub fn invoked_as_ssh() -> bool {
    std::env::args_os()
        .next()
        .and_then(|a| Path::new(&a).file_name().map(|n| n == "ssh"))
        .unwrap_or(false)
}

/// The real `ssh`: the first on `PATH` that isn't this wrapper, else `/usr/bin/ssh`.
fn real_ssh(path: &str, wrapper: &Path) -> PathBuf {
    let wrapper = std::fs::canonicalize(wrapper).unwrap_or_else(|_| wrapper.to_path_buf());
    let this = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join("ssh"))
        .find(|candidate| {
            let real = std::fs::canonicalize(candidate).ok();
            candidate.is_file() && real.as_ref() != Some(&wrapper) && real != this
        })
        .unwrap_or_else(|| PathBuf::from("/usr/bin/ssh"))
}

/// Runs the real `ssh` with the gate as its `ProxyCommand`. Returns only on failure.
pub fn exec_ssh() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let wrapper = args.next().map(PathBuf::from).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let ssh = real_ssh(&path, &wrapper);
    let piton = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(piton) => piton,
        Err(e) => return e,
    };
    let proxy = format!("ProxyCommand={} gate-connect %h %p", piton.display());
    std::process::Command::new(ssh).arg("-o").arg(proxy).args(args).exec()
}

/// `piton gate-connect HOST PORT`: a `CONNECT` tunnel through the gate in `HTTPS_PROXY`, on stdin and stdout.
pub fn connect(host: &str, port: u16) -> Result<(), String> {
    let url =
        std::env::var("HTTPS_PROXY").map_err(|_| "HTTPS_PROXY isn't set: no gate to connect through".to_string())?;
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("HTTPS_PROXY {url:?} isn't an http:// proxy"))?
        .trim_end_matches('/');
    let (auth, address) = match rest.rsplit_once('@') {
        Some((auth, address)) => (Some(auth), address),
        None => (None, rest),
    };
    let mut stream = TcpStream::connect(address).map_err(|e| format!("can't reach the gate at {address}: {e}"))?;
    let target = match host.contains(':') {
        true => format!("[{host}]:{port}"),
        false => format!("{host}:{port}"),
    };
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = auth {
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", STANDARD.encode(auth)));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            Ok(_) | Err(_) => return Err("the gate closed the connection".into()),
        }
        if head.len() > 16 * 1024 {
            return Err("the gate's answer is too long".into());
        }
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or_default();
    if status.split(' ').nth(1) != Some("200") {
        // The reason is the status phrase: what ssh prints.
        return Err(status.splitn(3, ' ').nth(2).unwrap_or(status).to_string());
    }
    let mut upload = stream.try_clone().map_err(|e| e.to_string())?;
    let up = std::thread::spawn(move || {
        let _ = relay(&mut std::io::stdin().lock(), &mut upload);
        let _ = upload.shutdown(std::net::Shutdown::Write);
    });
    let _ = relay(&mut stream, &mut std::io::stdout().lock());
    drop(up);
    Ok(())
}

/// Copies `from` to `to` chunk by chunk, flushing each: stdout is line-buffered, and SSH's binary key exchange has no
/// newlines, so `io::copy` would hold it back and both ends would wait for each other.
fn relay(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return to.flush();
        }
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each chunk reaches the writer when it's read, newline or not.
    #[test]
    fn the_relay_flushes_every_chunk() {
        struct Flushes(Vec<u8>, usize);
        impl Write for Flushes {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.1 += 1;
                Ok(())
            }
        }
        let mut out = Flushes(Vec::new(), 0);
        relay(&mut &b"\x00\x01binary-no-newline"[..], &mut out).unwrap();
        assert_eq!(out.0, b"\x00\x01binary-no-newline");
        assert!(out.1 >= 1, "flushed");
    }

    #[test]
    fn the_real_ssh_skips_the_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let (wrap, real) = (dir.path().join("wrap"), dir.path().join("real"));
        for d in [&wrap, &real] {
            std::fs::create_dir_all(d).unwrap();
        }
        let exe = std::env::current_exe().unwrap();
        std::os::unix::fs::symlink(&exe, wrap.join("ssh")).unwrap();
        std::fs::write(real.join("ssh"), "#!/bin/sh\n").unwrap();
        let path = format!("{}:{}", wrap.display(), real.display());
        assert_eq!(real_ssh(&path, &wrap.join("ssh")), real.join("ssh"));
        assert_eq!(
            real_ssh(&wrap.display().to_string(), &wrap.join("ssh")),
            PathBuf::from("/usr/bin/ssh")
        );
    }

    #[test]
    fn the_wrapper_links_to_this_binary() {
        let dir = tempfile::tempdir().unwrap();
        let folder = ensure(dir.path()).unwrap();
        let link = std::fs::read_link(folder.join("ssh")).unwrap();
        assert_eq!(link, std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap());
        assert_eq!(ensure(dir.path()), Some(folder), "again: unchanged");
    }
}
