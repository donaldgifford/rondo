//! Simple HTTP API for querying rondo metrics from the running VMM.
//!
//! Uses `std::net::TcpListener` — no external HTTP framework needed.
//! Endpoints:
//!
//! - `GET /metrics/health`  — liveness check
//! - `GET /metrics/info`    — store metadata (JSON)
//! - `GET /metrics/query?series=<name>&start=<ns>&end=<ns>` — time-series data (JSON)

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use crate::metrics::VmMetrics;

/// Runs the HTTP API server (blocking — intended for a dedicated thread).
pub fn run_api_server(metrics: Arc<Mutex<VmMetrics>>, port: u16) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("API bind failed on {addr}: {e}");
            return;
        }
    };

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
        };

        // Set a short read timeout so we don't block forever on slow clients
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

        if let Err(e) = handle_request(&stream, &metrics) {
            tracing::debug!("request error: {e}");
        }
    }
}

/// Parses an HTTP request and dispatches to the appropriate handler.
fn handle_request(
    stream: &std::net::TcpStream,
    metrics: &Arc<Mutex<VmMetrics>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Parse: "GET /path?query HTTP/1.x"
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return send_response(stream, 400, "Bad Request");
    }

    let (path, query) = match parts[1].split_once('?') {
        Some((p, q)) => (p, q),
        None => (parts[1], ""),
    };

    // Drain remaining headers (we don't need them)
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
    }

    match path {
        "/metrics/health" => send_response(stream, 200, r#"{"status":"ok"}"#),
        "/metrics/info" => handle_info(stream, metrics),
        "/metrics/query" => handle_query(stream, metrics, query),
        _ => send_response(stream, 404, r#"{"error":"not found"}"#),
    }
}

/// `GET /metrics/info` — returns store metadata.
fn handle_info(
    stream: &std::net::TcpStream,
    metrics: &Arc<Mutex<VmMetrics>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let m = metrics
        .lock()
        .map_err(|e| format!("lock: {e}"))?;
    let store = m.store();
    let handles = store.handles();

    let series_names: Vec<String> = handles
        .iter()
        .filter_map(|h| store.series_info(h).map(|(name, _labels)| name.to_string()))
        .collect();

    let body = serde_json::json!({
        "series_count": handles.len(),
        "series": series_names,
    });

    send_json(stream, 200, &body.to_string())
}

/// `GET /metrics/query?series=<name>&start=<ns>&end=<ns>` — returns data points.
fn handle_query(
    stream: &std::net::TcpStream,
    metrics: &Arc<Mutex<VmMetrics>>,
    query: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let params = parse_query(query);

    let series_name = params
        .get("series")
        .ok_or("missing 'series' parameter")?;
    let start: u64 = params
        .get("start")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let end: u64 = params
        .get("end")
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX);

    let m = metrics.lock().map_err(|e| format!("lock: {e}"))?;
    let store = m.store();

    // Find the series handle by name
    let handle = store
        .handles()
        .into_iter()
        .find(|h| {
            store
                .series_info(h)
                .is_some_and(|(name, _)| name == series_name.as_str())
        });

    let handle = match handle {
        Some(h) => h,
        None => {
            return send_json(
                stream,
                404,
                &format!(r#"{{"error":"series '{}' not found"}}"#, series_name),
            );
        }
    };

    // Query tier 0 (highest resolution)
    let result = store.query(handle, 0, start, end)?;
    let points: Vec<serde_json::Value> = result
        .map(|(ts, val)| {
            serde_json::json!({"t": ts, "v": val})
        })
        .collect();

    let body = serde_json::json!({
        "series": series_name,
        "tier": 0,
        "points": points,
    });

    send_json(stream, 200, &body.to_string())
}

/// Sends a plain-text HTTP response.
fn send_response(
    mut stream: &std::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };

    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    )?;

    Ok(())
}

/// Sends a JSON HTTP response.
fn send_json(
    stream: &std::net::TcpStream,
    status: u16,
    json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    send_response(stream, status, json)
}

/// Parses a query string into key-value pairs.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}
