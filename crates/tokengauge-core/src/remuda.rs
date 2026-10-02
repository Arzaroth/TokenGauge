//! Whether remuda's page is being served, so a frontend can offer to open it.
//!
//! The contract is remuda's and is recorded in ADR 0004: a running
//! `remuda serve` writes `serve.json` into its runtime directory and leaves it
//! there when it stops, so the file alone proves nothing. Only the same
//! process, its port still answering, counts. `serve.url` beside it carries
//! the page's access token and is never read here: `remuda open` is the only
//! way to the page.

use std::ffi::OsString;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{CredentialsConfig, TokenGaugeConfig};

pub const STATUS_FILE: &str = "serve.json";

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// What `serve.json` holds. No secret.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServeRecord {
    pub pid: u32,
    /// The process start time in clock ticks since boot, field 22 of
    /// `/proc/<pid>/stat`: with the pid, it names one process even after the
    /// pid is reused.
    pub started: u64,
    pub port: u16,
    pub version: String,
}

/// What `--json` carries as `remuda`, resolved on every render.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub serving: bool,
    pub version: Option<String>,
}

/// `$XDG_RUNTIME_DIR/remuda`, else the credential store, as remuda resolves
/// it. `None` with neither, which only a store turned off can produce.
pub fn runtime_dir(
    xdg_runtime_dir: Option<OsString>,
    store: &CredentialsConfig,
) -> Option<PathBuf> {
    xdg_runtime_dir
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("remuda"))
        .or_else(|| store.store_root())
}

/// remuda runs on Linux only, so elsewhere nothing is looked at.
pub fn status(config: &TokenGaugeConfig) -> Status {
    if !cfg!(target_os = "linux") {
        return Status::default();
    }
    runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"), &config.credentials)
        .map(|dir| status_in(&dir))
        .unwrap_or_default()
}

pub fn status_in(dir: &Path) -> Status {
    match serving_in(dir) {
        Some(record) => Status {
            serving: true,
            version: Some(record.version),
        },
        None => Status::default(),
    }
}

/// The record, when the process that wrote it is still the one at its pid and
/// its port still answers.
pub fn serving_in(dir: &Path) -> Option<ServeRecord> {
    let text = std::fs::read_to_string(dir.join(STATUS_FILE)).ok()?;
    let record: ServeRecord = serde_json::from_str(&text).ok()?;
    if started(record.pid) != Some(record.started) {
        return None;
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, record.port));
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    Some(record)
}

fn started(pid: u32) -> Option<u64> {
    let stat =
        std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    start_time(&stat)
}

/// Field 22 of a `/proc/<pid>/stat` line. The command name in field 2 may hold
/// spaces and parentheses, so the fields are counted from after its last `)`.
pub fn start_time(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-remuda-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn me() -> u64 {
        started(std::process::id()).expect("this process's start time")
    }

    fn write(dir: &Path, pid: u32, started: u64, port: u16) {
        let record = ServeRecord {
            pid,
            started,
            port,
            version: "0.5.0".into(),
        };
        std::fs::write(
            dir.join(STATUS_FILE),
            serde_json::to_string(&record).unwrap(),
        )
        .unwrap();
    }

    fn closed_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn the_serve_keys_are_spelled_as_the_adr_says() {
        let json = r#"{"pid": 42, "started": 123456, "port": 7429, "version": "0.5.0"}"#;
        let record: ServeRecord = serde_json::from_str(json).unwrap();
        assert_eq!(
            record,
            ServeRecord {
                pid: 42,
                started: 123456,
                port: 7429,
                version: "0.5.0".into(),
            }
        );
        assert_eq!(STATUS_FILE, "serve.json");
        let status = serde_json::to_value(Status {
            serving: true,
            version: Some("0.5.0".into()),
        })
        .unwrap();
        assert_eq!(
            status,
            serde_json::json!({"serving": true, "version": "0.5.0"})
        );
    }

    #[test]
    fn a_start_time_is_the_twenty_second_stat_field() {
        let stat = "1234 (a) b (c)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20 21";
        assert_eq!(start_time(stat), Some(987654));
        assert_eq!(start_time("1234 (short) S 1 2"), None);
        assert_eq!(start_time("no parenthesis"), None);

        let own = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<&str> = own.rsplit_once(')').unwrap().1.split_whitespace().collect();
        assert_eq!(start_time(&own), fields[19].parse().ok());
    }

    #[test]
    fn a_live_server_is_serving() {
        let dir = temp_dir("live");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        write(&dir, std::process::id(), me(), port);

        assert_eq!(
            status_in(&dir),
            Status {
                serving: true,
                version: Some("0.5.0".into()),
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_server_that_stopped_is_not_serving() {
        let dir = temp_dir("dead");
        assert_eq!(status_in(&dir), Status::default());
        assert_eq!(status_in(&dir.join("missing")), Status::default());

        std::fs::write(dir.join(STATUS_FILE), "{not json").unwrap();
        assert_eq!(status_in(&dir), Status::default());

        write(&dir, std::process::id(), me(), closed_port());
        assert_eq!(status_in(&dir), Status::default());

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        write(&dir, u32::MAX, me(), port);
        assert_eq!(status_in(&dir), Status::default());
        write(&dir, std::process::id(), me() + 1, port);
        assert_eq!(status_in(&dir), Status::default());
        write(&dir, std::process::id(), me(), port);
        assert!(status_in(&dir).serving);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_runtime_dir_is_xdg_runtime_else_the_store() {
        let store = CredentialsConfig {
            store: PathBuf::from("/data/remuda/credentials"),
            ..CredentialsConfig::off()
        };
        assert_eq!(
            runtime_dir(Some("/run/user/1000".into()), &store),
            Some(PathBuf::from("/run/user/1000/remuda"))
        );
        assert_eq!(
            runtime_dir(Some("".into()), &store),
            Some(PathBuf::from("/data/remuda/credentials"))
        );
        assert_eq!(
            runtime_dir(None, &store),
            Some(PathBuf::from("/data/remuda/credentials"))
        );
        assert_eq!(runtime_dir(None, &CredentialsConfig::off()), None);
    }
}
