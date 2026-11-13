//! How each file kind merges across levels, farthest first: `merge(base, nearer)`.

use super::files::{
    Branches, ClientPrefs, DaemonFile, EnvTable, HostPrefs, Keybindings, NetworkTable, PermissionsFile, PiTable,
    SandboxTable, SettingsFile, StopTable, TailnetTable, WebSearchFile, WorkersTable, WorkflowsFile,
};

/// Lists that only add: the base's entries, then the nearer's new ones.
fn union(base: &mut Vec<String>, nearer: Vec<String>) {
    for item in nearer {
        if !base.contains(&item) {
            base.push(item);
        }
    }
}

fn nearer<T>(base: &mut Option<T>, nearer: Option<T>) {
    if nearer.is_some() {
        *base = nearer;
    }
}

pub fn daemon(base: &mut DaemonFile, n: DaemonFile) {
    nearer(&mut base.socket, n.socket);
    nearer(&mut base.log, n.log);
    pi(&mut base.pi, n.pi);
    env(&mut base.env, n.env);
    stop(&mut base.stop, n.stop);
    tailnet(&mut base.tailnet, n.tailnet);
    sandbox(&mut base.sandbox, n.sandbox);
    workers(&mut base.workers, n.workers);
}

fn pi(base: &mut PiTable, n: PiTable) {
    nearer(&mut base.command, n.command);
    nearer(&mut base.args, n.args);
}

fn env(base: &mut EnvTable, n: EnvTable) {
    nearer(&mut base.login_shell, n.login_shell);
    nearer(&mut base.mise, n.mise);
}

fn stop(base: &mut StopTable, n: StopTable) {
    nearer(&mut base.grace_ms, n.grace_ms);
    nearer(&mut base.term_grace_ms, n.term_grace_ms);
}

fn tailnet(base: &mut TailnetTable, n: TailnetTable) {
    nearer(&mut base.enabled, n.enabled);
    nearer(&mut base.port, n.port);
    nearer(&mut base.cli, n.cli);
    nearer(&mut base.heartbeat_ms, n.heartbeat_ms);
    nearer(&mut base.retry_ms, n.retry_ms);
}

/// Grants only add; a nearer level can't remove a grant or a deny.
pub fn sandbox(base: &mut SandboxTable, n: SandboxTable) {
    nearer(&mut base.command, n.command);
    nearer(&mut base.base, n.base);
    union(&mut base.groups, n.groups);
    union(&mut base.allow, n.allow);
    union(&mut base.read, n.read);
    union(&mut base.read_file, n.read_file);
    union(&mut base.allow_file, n.allow_file);
    union(&mut base.deny, n.deny);
    union(&mut base.bypass, n.bypass);
    union(&mut base.unix_socket, n.unix_socket);
    network(&mut base.network, n.network);
}

fn network(base: &mut NetworkTable, n: NetworkTable) {
    for port in n.open_port {
        if !base.open_port.contains(&port) {
            base.open_port.push(port);
        }
    }
    nearer(&mut base.profile, n.profile);
}

fn workers(base: &mut WorkersTable, n: WorkersTable) {
    union(&mut base.skip, n.skip);
}

/// `settings.toml`, then `settings.local.toml`: the local value wins per key.
pub fn settings(base: &mut SettingsFile, n: SettingsFile) {
    host(&mut base.host, n.host);
    client(&mut base.client, n.client);
}

fn host(base: &mut HostPrefs, n: HostPrefs) {
    nearer(&mut base.roots, n.roots);
    nearer(&mut base.favorites, n.favorites);
    nearer(&mut base.logging, n.logging);
    nearer(&mut base.attach_mode, n.attach_mode);
    nearer(&mut base.empty_grace_ms, n.empty_grace_ms);
}

fn client(base: &mut ClientPrefs, n: ClientPrefs) {
    nearer(&mut base.text_font, n.text_font);
    nearer(&mut base.code_font, n.code_font);
}

/// Rule lists combine (deny and ask beat allow when evaluated); `defaultMode` and keybindings: the nearest file that
/// sets them.
pub fn permissions(base: &mut PermissionsFile, n: PermissionsFile) {
    nearer(&mut base.default_mode, n.default_mode);
    base.allow.extend(n.allow);
    base.ask.extend(n.ask);
    base.deny.extend(n.deny);
    keybindings(&mut base.keybindings, n.keybindings);
}

fn keybindings(base: &mut Keybindings, n: Keybindings) {
    nearer(&mut base.auto_accept_edits, n.auto_accept_edits);
}

pub fn web_search(base: &mut WebSearchFile, n: WebSearchFile) {
    nearer(&mut base.api_key, n.api_key);
}

pub fn workflows(base: &mut WorkflowsFile, n: WorkflowsFile) {
    branches(&mut base.branches, n.branches);
    nearer(&mut base.workflow_project, n.workflow_project);
    nearer(&mut base.session_analysis, n.session_analysis);
}

fn branches(base: &mut Branches, n: Branches) {
    nearer(&mut base.format, n.format);
    nearer(&mut base.username, n.username);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::files::{NetworkProfile, PermissionMode};

    #[test]
    fn sandbox_grants_only_add() {
        let mut base: SandboxTable = toml::from_str(
            "base = \"mise-dev\"\nallow = [\"/a\"]\ndeny = [\"/secret\"]\n[network]\nopen_port = [0]\nprofile = false\n",
        )
        .unwrap();
        let nearer: SandboxTable = toml::from_str("allow = [\"/a\", \"/b\"]\nread = [\"/r\"]\n").unwrap();
        sandbox(&mut base, nearer);
        assert_eq!(base.allow, ["/a", "/b"]);
        assert_eq!(base.read, ["/r"]);
        assert_eq!(base.deny, ["/secret"], "a nearer level can't drop a deny");
        assert_eq!(base.base.as_deref(), Some("mise-dev"));
        assert_eq!(base.network.open_port, [0]);
        assert_eq!(base.network.profile, Some(NetworkProfile::Off(false)));
    }

    #[test]
    fn permissions_combine_lists_and_take_the_nearest_default() {
        let mut base: PermissionsFile =
            serde_json::from_str(r#"{"defaultMode":"ask","deny":["bash(rm *)"],"allow":["read"]}"#).unwrap();
        permissions(
            &mut base,
            serde_json::from_str(r#"{"defaultMode":"allow","ask":["write"]}"#).unwrap(),
        );
        assert_eq!(base.default_mode, Some(PermissionMode::Allow));
        assert_eq!(base.deny, ["bash(rm *)"]);
        assert_eq!(base.ask, ["write"]);
        assert_eq!(base.allow, ["read"]);
        permissions(&mut base, PermissionsFile::default());
        assert_eq!(
            base.default_mode,
            Some(PermissionMode::Allow),
            "a file without defaultMode keeps it"
        );
    }

    #[test]
    fn scalars_take_the_nearer_value() {
        let mut base: DaemonFile = toml::from_str("[stop]\ngrace_ms = 1\nterm_grace_ms = 2\n").unwrap();
        daemon(&mut base, toml::from_str("[stop]\ngrace_ms = 9\n").unwrap());
        assert_eq!((base.stop.grace_ms, base.stop.term_grace_ms), (Some(9), Some(2)));
    }
}
