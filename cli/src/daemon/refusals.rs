//! The kernel's record of sandbox refusals: one `log stream` the daemon runs while any sandboxed session lives (outside
//! the sandbox; `log` refuses to run inside one). Each refusal names the refused process, its pid, the operation, and
//! the path or service, and ends with the launch's tag (`\npiton-<tag>`), which routes it to its session.
//!
//! macOS reports a process's first refusal and folds repeats (`N duplicate reports for ...`); docs/pi-findings.md has
//! what was verified.

use std::{process::Stdio, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::{launch::sandbox::TAG_PREFIX, session::SessionCmd};

/// One refusal, from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The launch's tag, without the prefix.
    pub tag: String,
    /// The refused process's name and pid.
    pub process: String,
    pub pid: i32,
    /// Seatbelt's operation: `file-write-create`, `file-read-data`, `mach-lookup`, `network-outbound`, ...
    pub operation: String,
    /// The path, service, or address, as the kernel names it.
    pub target: String,
}

impl Refusal {
    /// `operation target`, as the question shows it.
    pub fn summary(&self) -> String {
        format!("{} {}", self.operation, self.target)
    }
}

/// Reads one `eventMessage`: `[N duplicate reports for ]Sandbox: <process>(<pid>) deny(<n>) <operation> <target>\n<tag>`.
/// Anything else, or an untagged refusal (another app's), is `None`.
pub fn parse(message: &str) -> Option<Refusal> {
    let (body, tag) = message.rsplit_once('\n')?;
    let tag = tag.trim().strip_prefix(TAG_PREFIX)?.to_string();
    let body = match body.split_once("duplicate reports for ") {
        Some((_, rest)) => rest,
        None => body,
    };
    let rest = body.strip_prefix("Sandbox: ")?;
    let (process, rest) = rest.split_once('(')?;
    let (pid, rest) = rest.split_once(") ")?;
    let rest = rest.strip_prefix("deny(")?;
    let (_, rest) = rest.split_once(") ")?;
    let (operation, target) = rest.split_once(' ').unwrap_or((rest, ""));
    Some(Refusal {
        tag,
        process: process.to_string(),
        pid: pid.parse().ok()?,
        operation: operation.to_string(),
        target: target.to_string(),
    })
}

/// The `log stream` arguments: the Sandbox kernel extension's reports that carry a Piton tag.
fn arguments() -> Vec<String> {
    vec![
        "stream".into(),
        "--style".into(),
        "ndjson".into(),
        "--predicate".into(),
        format!("senderImagePath ENDSWITH \"/Sandbox\" AND eventMessage CONTAINS \"\\n{TAG_PREFIX}\""),
    ]
}

/// Runs `log stream` for as long as the daemon lives, restarting it when it exits, and sends each refusal to its
/// session (by tag).
pub async fn watch(daemon: Arc<super::Daemon>) {
    loop {
        match stream(&daemon).await {
            Ok(()) => crate::log!("sandbox refusals: log stream ended; restarting"),
            Err(e) => crate::log!("sandbox refusals: can't read the kernel's log: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn stream(daemon: &super::Daemon) -> std::io::Result<()> {
    let mut child = Command::new("/usr/bin/log")
        .args(arguments())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        return Ok(());
    };
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(serde_json::Value::Object(record)) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(refusal) = record
            .get("eventMessage")
            .and_then(serde_json::Value::as_str)
            .and_then(parse)
        else {
            continue;
        };
        if let Some(session) = daemon.session_by_tag(&refusal.tag) {
            let _ = session.tx.send(SessionCmd::Refusal(refusal));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forms the spike recorded on macOS 27 (`scratch/spike` of #241), with a tag added.
    #[test]
    fn reads_the_kernel_s_refusals() {
        assert_eq!(
            parse("Sandbox: touch(30043) deny(1) file-write-create /Users/A/x y\npiton-3-ab12"),
            Some(Refusal {
                tag: "3-ab12".into(),
                process: "touch".into(),
                pid: 30043,
                operation: "file-write-create".into(),
                target: "/Users/A/x y".into(),
            })
        );
        let keychain = parse("Sandbox: security(29911) deny(1) mach-lookup com.apple.SecurityServer\npiton-t").unwrap();
        assert_eq!(
            (keychain.operation.as_str(), keychain.target.as_str()),
            ("mach-lookup", "com.apple.SecurityServer")
        );
        let repeated =
            parse("27 duplicate reports for Sandbox: node(5) deny(1) file-read-data /h/.ssh/id\npiton-t").unwrap();
        assert_eq!((repeated.process.as_str(), repeated.pid), ("node", 5));
        let defaults = parse("Sandbox: defaults(30128) deny(1) user-preference-write com.piton.x\npiton-t").unwrap();
        assert_eq!(defaults.summary(), "user-preference-write com.piton.x");
        // Other apps' refusals and Piton's untagged ones are ignored.
        assert_eq!(
            parse("Sandbox: WhatsApp(93922) deny(1) system-info vfs.disk-space"),
            None
        );
        assert_eq!(
            parse("Sandbox: touch(1) deny(1) file-write-create /x\nPITONSPIKE-TAG"),
            None
        );
        assert_eq!(parse("Sandbox apply: piton[29829] <bytes>\npiton-t"), None);
        assert!(arguments().iter().any(|a| a.contains("piton-")));
    }
}
