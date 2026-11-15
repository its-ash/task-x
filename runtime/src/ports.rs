//! `ports/list` and `ports/stop`: the TCP ports that processes task-x started listen on (a dev
//! server, a preview, a debugger), so a client can show which ports are taken and stop them.
//!
//! A listening process counts when it descends from this runtime (`run_command`, Claude Code and
//! its shells, MCP servers), or when its working directory is inside the open workspace: a server
//! started with `&` or `nohup` leaves the process tree once its shell ends. The runtime itself and
//! its ancestors (the editor) are never listed, and only a listed process can be stopped.
//!
//! Sockets come from `lsof` (macOS, most Linux systems) or `ss` (Linux); without either the list is
//! `supported: false`.

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest command line shown for a process.
const MAX_COMMAND: usize = 300;
/// How long a stopped process gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Why a port is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    /// Started by task-x: the agent's commands, Claude Code's shells, MCP servers.
    TaskX,
    /// Not under task-x, but running in the workspace folder (often a server task-x started in the background).
    Workspace,
}

/// One port in use: every address its process listens on for that port.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    pub port: u16,
    pub addresses: Vec<String>,
    pub pid: u32,
    pub name: String,
    pub command: String,
    pub origin: Origin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// One listening socket.
#[derive(Debug, Clone, PartialEq)]
struct Listener {
    pid: u32,
    name: String,
    address: String,
    port: u16,
}

struct Proc {
    ppid: u32,
    command: String,
}

/// The standard output of a program; `None` when it cannot run. `lsof` exits with 1 when nothing
/// matches, so the exit status is not checked.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `127.0.0.1:3000`, `*:3000`, `[::1]:3000`, `127.0.0.53%lo:53` → host and port.
fn split_address(s: &str) -> Option<(String, u16)> {
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.split('%').next().unwrap_or(host);
    Some((host.to_owned(), port))
}

/// `lsof -F pcn` output: a `p<pid>` line, a `c<name>` line, then `f`/`n<address>` lines per socket.
fn parse_lsof(out: &str) -> Vec<Listener> {
    let (mut pid, mut name, mut found) = (0u32, String::new(), Vec::new());
    for line in out.lines() {
        let mut chars = line.chars();
        let tag = chars.next();
        let rest = chars.as_str();
        match tag {
            Some('p') => {
                pid = rest.parse().unwrap_or(0);
                name.clear();
            }
            Some('c') => rest.clone_into(&mut name),
            Some('n') if pid > 0 => {
                if let Some((address, port)) = split_address(rest) {
                    found.push(Listener { pid, name: name.clone(), address, port });
                }
            }
            _ => {}
        }
    }
    found
}

/// `ss -ltnpH` output: `LISTEN 0 511 127.0.0.1:3000 0.0.0.0:* users:(("node",pid=1234,fd=20))`.
/// Sockets of other users' processes have no `users:` part and are skipped.
fn parse_ss(out: &str) -> Vec<Listener> {
    let users = regex::Regex::new(r#"\("([^"]*)",pid=(\d+)"#).expect("valid regex");
    out.lines()
        .flat_map(|line| {
            let local = line.split_whitespace().nth(3).and_then(split_address);
            let Some((address, port)) = local else { return Vec::new() };
            users
                .captures_iter(line)
                .filter_map(|c| Some(Listener { pid: c[2].parse().ok()?, name: c[1].to_owned(), address: address.clone(), port }))
                .collect()
        })
        .collect()
}

/// Every listening TCP socket this user can see; `None` when neither `lsof` nor `ss` runs.
fn listeners() -> Option<Vec<Listener>> {
    if let Some(out) = output("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]) {
        return Some(parse_lsof(&out));
    }
    output("ss", &["-ltnpH"]).map(|o| parse_ss(&o))
}

/// `ps -A -o pid=,ppid=,command=` output by pid.
fn parse_ps(out: &str) -> HashMap<u32, Proc> {
    out.lines()
        .filter_map(|l| {
            let (pid, rest) = l.trim_start().split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Some((pid.parse().ok()?, Proc { ppid: ppid.parse().ok()?, command: command.trim().to_owned() }))
        })
        .collect()
}

fn processes() -> HashMap<u32, Proc> {
    output("ps", &["-A", "-o", "pid=,ppid=,command="]).map(|o| parse_ps(&o)).unwrap_or_default()
}

/// Every process under `root` (not `root` itself).
fn descendants(root: u32, procs: &HashMap<u32, Proc>) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, p) in procs {
        children.entry(p.ppid).or_default().push(*pid);
    }
    let (mut found, mut todo) = (HashSet::new(), vec![root]);
    while let Some(p) = todo.pop() {
        for c in children.get(&p).into_iter().flatten() {
            if *c != root && found.insert(*c) {
                todo.push(*c);
            }
        }
    }
    found
}

/// The processes above `pid`, up to the first one (`launchd`/`init`).
fn ancestors(pid: u32, procs: &HashMap<u32, Proc>) -> HashSet<u32> {
    let (mut found, mut at) = (HashSet::new(), pid);
    while let Some(p) = procs.get(&at) {
        if p.ppid <= 1 || !found.insert(p.ppid) {
            break;
        }
        at = p.ppid;
    }
    found
}

/// The working directories of `pids`.
#[cfg(target_os = "linux")]
fn cwds(pids: &[u32]) -> HashMap<u32, String> {
    pids.iter().filter_map(|p| Some((*p, std::fs::read_link(format!("/proc/{p}/cwd")).ok()?.to_string_lossy().into_owned()))).collect()
}

/// The working directories of `pids`.
#[cfg(not(target_os = "linux"))]
fn cwds(pids: &[u32]) -> HashMap<u32, String> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let Some(out) = output("lsof", &["-a", "-d", "cwd", "-Fpn", "-p", &list]) else { return HashMap::new() };
    let (mut pid, mut found) = (0u32, HashMap::new());
    for line in out.lines() {
        let mut chars = line.chars();
        match chars.next() {
            Some('p') => pid = chars.as_str().parse().unwrap_or(0),
            Some('n') if pid > 0 => {
                found.insert(pid, chars.as_str().to_owned());
            }
            _ => {}
        }
    }
    found
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_COMMAND) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// The listed ports among `found`: those of processes under `me`, then those running in `root`.
fn collect(found: Vec<Listener>, procs: &HashMap<u32, Proc>, me: u32, root: Option<&Path>, cwds: impl Fn(&[u32]) -> HashMap<u32, String>) -> Vec<PortInfo> {
    let ours = descendants(me, procs);
    let above = ancestors(me, procs);
    // The workspace path as given and as resolved (macOS `/tmp` is `/private/tmp`).
    let roots: Vec<PathBuf> = root.map_or_else(Vec::new, |r| {
        let mut v = vec![r.to_path_buf()];
        v.extend(r.canonicalize().ok().filter(|c| c != r));
        v
    });
    let mut others: Vec<u32> = found.iter().map(|l| l.pid).filter(|p| *p != me && !ours.contains(p) && !above.contains(p)).collect();
    others.sort_unstable();
    others.dedup();
    let dirs = if roots.is_empty() { HashMap::new() } else { cwds(&others) };
    let mut ports: BTreeMap<(u16, u32), PortInfo> = BTreeMap::new();
    for l in found {
        if l.pid == me || above.contains(&l.pid) {
            continue;
        }
        let cwd = dirs.get(&l.pid).cloned();
        let origin = if ours.contains(&l.pid) {
            Origin::TaskX
        } else if cwd.as_deref().is_some_and(|c| roots.iter().any(|r| Path::new(c).starts_with(r))) {
            Origin::Workspace
        } else {
            continue;
        };
        let e = ports.entry((l.port, l.pid)).or_insert_with(|| PortInfo {
            port: l.port,
            addresses: Vec::new(),
            pid: l.pid,
            command: clip(procs.get(&l.pid).map_or(l.name.as_str(), |p| p.command.as_str())),
            name: l.name.clone(),
            origin,
            cwd,
        });
        if !e.addresses.contains(&l.address) {
            e.addresses.push(l.address);
        }
    }
    ports.into_values().collect()
}

/// The listed ports now; `None` when neither `lsof` nor `ss` runs. Blocking.
pub fn current(root: Option<&Path>) -> Option<Vec<PortInfo>> {
    listeners().map(|found| collect(found, &processes(), std::process::id(), root, cwds))
}

/// `ports/list`: `{ supported, ports }`. Blocking (it runs `lsof` and `ps`).
pub fn list(root: Option<&Path>) -> Value {
    match current(root) {
        Some(ports) => json!({ "supported": true, "ports": ports }),
        None => json!({ "supported": false, "ports": [] }),
    }
}

/// Whether `pid` still runs (a zombie waiting for its parent does not).
fn alive(pid: u32) -> bool {
    output("ps", &["-o", "stat=", "-p", &pid.to_string()]).is_some_and(|s| !s.trim().is_empty() && !s.trim_start().starts_with('Z'))
}

fn signal(sig: &str, pids: &[u32]) {
    let _ = Command::new("kill").arg(format!("-{sig}")).args(pids.iter().map(u32::to_string)).stderr(Stdio::null()).status();
}

/// `ports/stop`: ends a listed port's process and every process under it, with SIGTERM and, after
/// `STOP_GRACE`, SIGKILL. `{ stopped, forced }`; an error for a process that is not listed now.
/// Blocking.
pub fn stop(pid: u32, root: Option<&Path>) -> Result<Value, String> {
    let ports = current(root).ok_or("listing ports needs lsof or ss, and neither runs here")?;
    let Some(p) = ports.iter().find(|p| p.pid == pid) else {
        return Err(format!("process {pid} is not using any port task-x lists (see ports/list)"));
    };
    let mut doomed = vec![pid];
    doomed.extend(descendants(pid, &processes()));
    signal("TERM", &doomed);
    let deadline = Instant::now() + STOP_GRACE;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let forced = alive(pid);
    if forced {
        signal("KILL", &doomed);
        let deadline = Instant::now() + Duration::from_secs(1);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(json!({ "stopped": !alive(pid), "forced": forced, "port": p.port, "name": p.name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs(rows: &[(u32, u32, &str)]) -> HashMap<u32, Proc> {
        rows.iter().map(|(p, pp, c)| (*p, Proc { ppid: *pp, command: (*c).to_owned() })).collect()
    }

    #[test]
    fn reads_lsof_ss_and_ps_output() {
        let lsof = "p501\ncnode\nf23\nn*:5173\nf24\nn[::1]:5173\np77\ncpython3\nf3\nn127.0.0.1:8000\n";
        let l = parse_lsof(lsof);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].pid, l[0].name.as_str(), l[0].address.as_str(), l[0].port), (501, "node", "*", 5173));
        assert_eq!((l[1].address.as_str(), l[2].pid, l[2].port), ("::1", 77, 8000));
        let ss = "LISTEN 0 511 127.0.0.1:3000 0.0.0.0:* users:((\"node\",pid=1234,fd=20))\nLISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:*\nLISTEN 0 128 [::]:8080 [::]:* users:((\"java\",pid=9,fd=5),(\"java\",pid=10,fd=5))\n";
        let s = parse_ss(ss);
        assert_eq!(s.iter().map(|l| (l.pid, l.port, l.address.as_str())).collect::<Vec<_>>(), [(1234, 3000, "127.0.0.1"), (9, 8080, "::"), (10, 8080, "::")]);
        let ps = parse_ps("    1     0 /sbin/launchd\n  501   400 node /app/node_modules/.bin/vite --port 5173\n");
        assert_eq!((ps[&501].ppid, ps[&501].command.as_str()), (400, "node /app/node_modules/.bin/vite --port 5173"));
    }

    #[test]
    fn lists_what_task_x_started_or_what_runs_in_the_workspace() {
        // 1 launchd → 10 editor → 20 runtime (me) → 30 shell → 40 vite; 50 a server left in the workspace;
        // 60 elsewhere; 10 (the editor) and 20 (the runtime) listen too.
        let p = procs(&[(1, 0, "launchd"), (10, 1, "Code"), (20, 10, "task-x-runtime"), (30, 20, "sh -c npm run dev"), (40, 30, "node vite"), (50, 1, "python3 -m http.server"), (60, 1, "postgres")]);
        let l = |pid: u32, port: u16, address: &str| Listener { pid, name: format!("p{pid}"), address: address.into(), port };
        let found = vec![l(40, 5173, "*"), l(40, 5173, "::1"), l(50, 8000, "127.0.0.1"), l(60, 5432, "127.0.0.1"), l(10, 9229, "127.0.0.1"), l(20, 4000, "127.0.0.1")];
        let dirs = |pids: &[u32]| -> HashMap<u32, String> {
            assert!(!pids.contains(&40) && !pids.contains(&10), "only processes outside task-x and the editor are looked up");
            HashMap::from([(50, "/work/app/site".to_owned()), (60, "/var/db".to_owned())])
        };
        let ports = collect(found, &p, 20, Some(Path::new("/work/app")), dirs);
        let rows: Vec<_> = ports.iter().map(|p| (p.port, p.pid, p.origin, p.addresses.join(","))).collect();
        assert_eq!(rows, [(5173, 40, Origin::TaskX, "*,::1".to_owned()), (8000, 50, Origin::Workspace, "127.0.0.1".to_owned())]);
        assert_eq!(ports[0].command, "node vite");
        assert_eq!(ports[1].cwd.as_deref(), Some("/work/app/site"));
        // With no workspace open, only task-x's own processes count.
        let none = collect(vec![l(50, 8000, "127.0.0.1"), l(40, 5173, "*")], &p, 20, None, |_| unreachable!("no workspace, no lookup"));
        assert_eq!(none.iter().map(|p| p.pid).collect::<Vec<_>>(), [40]);
    }

    /// A real child of this process listening on a port is listed and then stopped.
    #[test]
    fn a_listening_child_is_listed_and_stopped() {
        if listeners().is_none() {
            return; // no lsof or ss here
        }
        let script = "use IO::Socket::INET; my $s = IO::Socket::INET->new(Listen => 5, LocalAddr => '127.0.0.1', LocalPort => 0) or die; sleep 60;";
        let Ok(mut child) = Command::new("perl").args(["-e", script]).stdout(Stdio::null()).spawn() else { return };
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        let port = loop {
            if let Some(p) = current(None).and_then(|ports| ports.into_iter().find(|p| p.pid == pid)) {
                break p;
            }
            assert!(Instant::now() < deadline, "the child's port was never listed");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!((port.origin, port.addresses.as_slice()), (Origin::TaskX, ["127.0.0.1".to_owned()].as_slice()));
        assert!(port.command.contains("perl"));
        assert!(stop(1, None).is_err(), "only a listed process can be stopped");
        let r = stop(pid, None).unwrap();
        assert_eq!((r["stopped"].as_bool(), r["port"].as_u64()), (Some(true), Some(u64::from(port.port))), "{r}");
        assert!(child.wait().is_ok());
        assert!(current(None).unwrap().iter().all(|p| p.pid != pid), "the port is free again");
    }
}
