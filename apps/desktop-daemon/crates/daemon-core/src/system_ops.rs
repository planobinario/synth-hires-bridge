//! Bounded host operations exposed by the Desktop bridge.
//!
//! These operations deliberately avoid shell interpolation. Process control
//! uses the platform process APIs/commands with discrete arguments, network
//! responses are capped before being returned to the server, and filesystem
//! watching has a finite duration so one action cannot occupy the WS forever.

use crate::{DaemonError, Result};
use futures_util::StreamExt;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::process::Command;

const DEFAULT_PROCESS_LIMIT: usize = 200;
const MAX_PROCESS_LIMIT: usize = 1000;
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_WATCH_DURATION_MS: u64 = 120_000;

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessListRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessListResult {
    pub processes: Vec<ProcessInfo>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessKillRequest {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessKillResult {
    pub killed: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkFetchRequest {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkFetchResult {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsWatchRequest {
    pub path: PathBuf,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsWatchEvent {
    pub kind: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsWatchResult {
    pub events: Vec<FsWatchEvent>,
    pub duration_ms: u64,
}

fn default_method() -> String {
    "GET".to_string()
}

pub async fn list_processes(req: ProcessListRequest) -> Result<ProcessListResult> {
    let limit = req
        .limit
        .unwrap_or(DEFAULT_PROCESS_LIMIT)
        .clamp(1, MAX_PROCESS_LIMIT);
    let query = req.query.unwrap_or_default().to_lowercase();

    #[cfg(target_os = "windows")]
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .await
        .map_err(DaemonError::Io)?;

    #[cfg(not(target_os = "windows"))]
    let output = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output()
        .await
        .map_err(DaemonError::Io)?;

    if !output.status.success() {
        return Err(DaemonError::Protocol(format!(
            "process listing failed with {}",
            output.status
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut processes = Vec::new();
    for line in text.lines() {
        let parsed = parse_process_line(line);
        let Some(process) = parsed else { continue };
        let searchable = format!("{} {}", process.name, process.command).to_lowercase();
        if !query.is_empty() && !searchable.contains(&query) {
            continue;
        }
        processes.push(process);
        if processes.len() >= limit {
            break;
        }
    }

    Ok(ProcessListResult {
        truncated: processes.len() >= limit,
        processes,
    })
}

#[cfg(target_os = "windows")]
fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let fields: Vec<String> = line
        .split("\",\"")
        .map(|field| field.trim_matches('"').to_string())
        .collect();
    let name = fields.first()?.clone();
    let pid = fields.get(1)?.parse().ok()?;
    Some(ProcessInfo {
        pid,
        name: name.clone(),
        command: name,
    })
}

#[cfg(not(target_os = "windows"))]
fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let name = fields.next()?.to_string();
    let command = fields.collect::<Vec<_>>().join(" ");
    Some(ProcessInfo { pid, name, command })
}

pub async fn kill_process(req: ProcessKillRequest) -> Result<ProcessKillResult> {
    if req.pid.is_some() == req.name.is_some() {
        return Err(DaemonError::Protocol(
            "provide exactly one of pid or name".to_string(),
        ));
    }

    let pids = if let Some(pid) = req.pid {
        if pid == std::process::id() {
            return Err(DaemonError::Protocol(
                "refusing to terminate the daemon itself".into(),
            ));
        }
        vec![pid]
    } else {
        let name = req.name.unwrap_or_default().trim().to_lowercase();
        if name.is_empty() || name.len() > 255 {
            return Err(DaemonError::Protocol("invalid process name".into()));
        }
        let listed = list_processes(ProcessListRequest {
            query: Some(name.clone()),
            limit: Some(MAX_PROCESS_LIMIT),
        })
        .await?;
        listed
            .processes
            .into_iter()
            .filter(|p| p.name.to_lowercase() == name && p.pid != std::process::id())
            .map(|p| p.pid)
            .take(32)
            .collect()
    };

    let mut killed = Vec::new();
    for pid in pids {
        #[cfg(target_os = "windows")]
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .await
            .map_err(DaemonError::Io)?;

        #[cfg(not(target_os = "windows"))]
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await
            .map_err(DaemonError::Io)?;

        if status.success() {
            killed.push(pid);
        }
    }
    Ok(ProcessKillResult { killed })
}

pub async fn fetch_network(req: NetworkFetchRequest) -> Result<NetworkFetchResult> {
    let parsed = reqwest::Url::parse(&req.url)
        .map_err(|e| DaemonError::Protocol(format!("invalid url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DaemonError::Protocol(
            "only http and https are supported".into(),
        ));
    }
    // Cloud metadata SSRF guard: the daemon runs with the USER's cloud
    // credentials nearby (~/aws, gcloud, az). A compromised server must
    // not turn network.fetch into an IMDS credential oracle. Loopback and
    // RFC1918 stay allowed (local dev servers are a legitimate use) —
    // documented limitation: DNS-rebinding a public name to 169.254.x
    // is NOT resolved/checked here (no blocking DNS in the hot path).
    if is_cloud_metadata_host(parsed.host_str().unwrap_or("")) {
        return Err(DaemonError::Protocol(
            "cloud metadata endpoints are blocked".into(),
        ));
    }

    let timeout_ms = req.timeout_ms.unwrap_or(30_000).clamp(100, 120_000);
    let max_bytes = req
        .max_bytes
        .unwrap_or(MAX_FETCH_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let method = req
        .method
        .parse::<reqwest::Method>()
        .map_err(|e| DaemonError::Protocol(format!("invalid method: {e}")))?;
    let mut headers = HeaderMap::new();
    for (name, value) in req.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| DaemonError::Protocol(format!("invalid header name: {e}")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|e| DaemonError::Protocol(format!("invalid header value: {e}")))?;
        headers.insert(name, value);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| DaemonError::Protocol(format!("http client: {e}")))?;
    let mut request = client.request(method, parsed).headers(headers);
    if let Some(body) = req.body {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| DaemonError::Protocol(format!("network fetch: {e}")))?;
    let status = response.status().as_u16();
    let response_headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();

    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DaemonError::Protocol(format!("network body: {e}")))?;
        let remaining = max_bytes.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= max_bytes {
            truncated = true;
            break;
        }
    }

    Ok(NetworkFetchResult {
        status,
        headers: response_headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        bytes: bytes.len(),
        truncated,
    })
}

/// Cloud instance-metadata hosts: credential oracles, never fetch targets.
fn is_cloud_metadata_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    // Strip brackets (IPv6) and trailing dot (FQDN form).
    let h = h.trim_start_matches('[').trim_end_matches(']');
    let h = h.trim_end_matches('.');
    matches!(
        h,
        // AWS / GCP / Azure / Oracle / DigitalOcean link-local metadata.
        "169.254.169.254" | "fd00:ec2::254"
        // Alibaba Cloud metadata.
        | "100.100.100.200"
        // GCP metadata DNS name.
        | "metadata.google.internal"
        // Azure wired-server / platform endpoints reachable link-local.
        | "168.63.129.16"
    )
}

pub async fn watch_filesystem(req: FsWatchRequest) -> Result<FsWatchResult> {
    let duration_ms = req
        .duration_ms
        .unwrap_or(10_000)
        .clamp(100, MAX_WATCH_DURATION_MS);
    let path = req.path;
    let recursive = req.recursive;
    let started = Instant::now();

    tokio::task::spawn_blocking(move || {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let mut watcher = recommended_watcher(move |event| {
            let _ = events_tx.send(event);
        })
        .map_err(|e| DaemonError::Protocol(format!("watcher: {e}")))?;
        watcher
            .watch(
                &path,
                if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|e| DaemonError::Protocol(format!("watch path: {e}")))?;

        let deadline = Instant::now() + Duration::from_millis(duration_ms);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events_rx.recv_timeout(remaining) {
                Ok(Ok(event)) => events.push(FsWatchEvent {
                    kind: format!("{:?}", event.kind),
                    paths: event
                        .paths
                        .into_iter()
                        .map(|p| p.display().to_string())
                        .collect(),
                }),
                Ok(Err(error)) => {
                    return Err(DaemonError::Protocol(format!("watch event: {error}")))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(FsWatchResult {
            events,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    })
    .await
    .map_err(|e| DaemonError::Protocol(format!("watch task: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::is_cloud_metadata_host;

    #[test]
    fn metadata_hosts_blocked() {
        assert!(is_cloud_metadata_host("169.254.169.254"));
        assert!(is_cloud_metadata_host("100.100.100.200"));
        assert!(is_cloud_metadata_host("metadata.google.internal"));
        assert!(is_cloud_metadata_host("metadata.google.internal."));
        assert!(is_cloud_metadata_host("168.63.129.16"));
        assert!(is_cloud_metadata_host("[fd00:ec2::254]"));
    }

    #[test]
    fn legitimate_hosts_allowed() {
        assert!(!is_cloud_metadata_host("127.0.0.1"));
        assert!(!is_cloud_metadata_host("localhost"));
        assert!(!is_cloud_metadata_host("192.168.1.10"));
        assert!(!is_cloud_metadata_host("synthhires.com"));
        assert!(!is_cloud_metadata_host("registry.npmjs.org"));
    }
}
