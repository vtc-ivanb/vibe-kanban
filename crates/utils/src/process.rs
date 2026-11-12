use command_group::AsyncGroupChild;
#[cfg(unix)]
use tokio::time::Duration;

pub async fn kill_process_group(child: &mut AsyncGroupChild) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        // Use command_group's UnixChildExt::signal() which calls killpg()
        // with the pgid captured at spawn time. This works even after the
        // group leader has exited, unlike getpgid() which would fail.
        use command_group::{Signal, UnixChildExt};

        for sig in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGKILL] {
            tracing::info!("Sending {:?} to process group", sig);
            if let Err(e) = child.signal(sig) {
                // break if the group does not exist anymore
                if e.raw_os_error() == Some(nix::libc::ESRCH) {
                    break;
                }
                tracing::warn!("Failed to send signal {:?} to process group: {}", sig, e);
            }
            if sig != Signal::SIGKILL {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
    Ok(())
}

/// Terminate each process group whose leader has one of these process IDs.
/// Missing groups are ignored; other signal failures are logged.
pub async fn kill_process_groups(pids: &[u32]) -> std::io::Result<()> {
    if pids.is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };

        for sig in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGKILL] {
            for pid in pids {
                let Ok(pid) = i32::try_from(*pid) else {
                    continue;
                };
                if let Err(e) = killpg(Pid::from_raw(pid), sig)
                    && e != Errno::ESRCH
                {
                    tracing::warn!(
                        "Failed to send signal {:?} to process group {}: {}",
                        sig,
                        pid,
                        e
                    );
                }
            }
            if sig != Signal::SIGKILL {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pids;
    Ok(())
}

/// Whether `pid` is a live (non-zombie) descendant of `ancestor`. Checking
/// ancestry rather than bare existence keeps a reused pid from matching.
/// Always `false` on non-unix platforms, where the check is not implemented.
pub fn is_live_descendant(pid: u32, ancestor: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(output) = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "stat="])
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let mut parents = std::collections::HashMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(Ok(id)), Some(Ok(parent))) = (
                fields.next().map(str::parse::<u32>),
                fields.next().map(str::parse::<u32>),
            ) else {
                continue;
            };
            if id == pid && fields.next().is_some_and(|stat| stat.starts_with('Z')) {
                return false;
            }
            parents.insert(id, parent);
        }
        let mut current = pid;
        // Bounded walk in case the snapshot contains a cycle.
        for _ in 0..parents.len() {
            match parents.get(&current) {
                Some(&parent) if parent == ancestor => return true,
                Some(&parent) if parent > 1 && parent != current => current = parent,
                _ => return false,
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, ancestor);
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn matches_only_live_descendants() {
        let me = std::process::id();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(is_live_descendant(pid, me));
        // An unrelated process is not a descendant of the child.
        assert!(!is_live_descendant(me, pid));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_live_descendant(pid, me));
    }
}
