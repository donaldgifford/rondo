# Rondo Deploy

Kubernetes manifests for the rondo VMM metrics pipeline.

## Metrics Pipeline

```
rondo-demo-vmm (10.10.11.33:9100)
  └─ export thread (every 10s)
       └─ drain tier 0 → protobuf + snappy → HTTP POST
            └─ Prometheus (remote-write receiver)
                 └─ Grafana (dashboard via grafana-operator)
```

The demo VMM pushes metrics to Prometheus via the remote-write protocol. No scrape configuration is needed — the VMM is the active sender.

## Prerequisites

- Kubernetes cluster with:
  - [kube-prometheus-stack](https://github.com/prometheus-community/helm-charts/tree/main/charts/kube-prometheus-stack) in `monitoring` namespace
  - [grafana-operator](https://github.com/grafana/grafana-operator) with `GrafanaDashboard` CRD support
- Prometheus remote-write receiver enabled (`--web.enable-remote-write-receiver`)
- Prometheus accessible from the VMM box (currently via `https://prometheus.fartlab.dev`)

## Deploy the Dashboard

```bash
kubectl apply -f deploy/k8s/
```

This creates:
- `ConfigMap/rondo-vmm-dashboard` — the Grafana dashboard JSON
- `GrafanaDashboard/rondo-vmm-dashboard` — tells the grafana-operator to load the dashboard

The dashboard appears in the **Rondo** folder in Grafana.

## Run the VMM with Remote-Write

```bash
# From the project root (uses Makefile to sync, build, and run on remote box)
make vmm-demo-remote-write
```

This runs a 45-second VM workload on the remote box with `--remote-write` pointing to the Prometheus endpoint configured in `VMM_REMOTE_WRITE` (Makefile variable).

To override the endpoint:

```bash
make vmm-demo-remote-write VMM_REMOTE_WRITE=https://your-prometheus/api/v1/write
```

## Dashboard Panels

| Panel | Metric | Description |
|-------|--------|-------------|
| VMM Uptime | `vmm_uptime_seconds` | Seconds since VMM started |
| RSS Memory | `vmm_rss_bytes` | Resident set size from /proc/self/status |
| Open FDs | `vmm_open_fds` | Open file descriptor count |
| vCPU Exits by Reason | `vcpu_exits_total{reason=...}` | Exit counts by reason (io, mmio, hlt, shutdown, other) |
| vCPU Exit Duration | `vcpu_exit_duration_ns` | Time processing each vCPU exit |
| vCPU Run Duration | `vcpu_run_duration_ns` | Time spent in KVM_RUN between exits |

## Metrics Reference

The VMM registers 16 series across three categories:

**vCPU metrics** (recorded on every KVM exit):
- `vcpu_exits_total{reason=io|mmio|hlt|shutdown|other}` — exit count by reason
- `vcpu_exit_duration_ns` — nanoseconds processing each exit
- `vcpu_run_duration_ns` — nanoseconds in KVM_RUN

**Block device metrics** (recorded on virtio-blk I/O, not yet active):
- `blk_requests_total{op=read|write|flush}` — request count by operation
- `blk_bytes_total{direction=read|write}` — bytes transferred
- `blk_request_duration_ns` — nanoseconds per request

**Process metrics** (recorded by 1Hz maintenance thread):
- `vmm_rss_bytes` — resident memory
- `vmm_open_fds` — open file descriptors
- `vmm_uptime_seconds` — VMM uptime

## Remote-Write Details

- **Protocol**: Prometheus remote-write v1 (protobuf + snappy compression)
- **Push interval**: Every 10 seconds
- **Tier exported**: Tier 0 (1-second resolution raw data)
- **Cursor persistence**: `vmm_metrics/cursor_prometheus.json` tracks export progress for incremental delivery
- **Retry**: Exponential backoff (100ms initial, 3 retries)

## Troubleshooting

### No data in Prometheus / Grafana

#### 1. Enable the remote-write receiver on Prometheus

The remote-write receiver is **not enabled by default**. If you're using kube-prometheus-stack, set it in your Helm values:

```yaml
prometheus:
  prometheusSpec:
    enableRemoteWriteReceiver: true
```

Or pass `--web.enable-remote-write-receiver` to the Prometheus binary directly.

Verify the endpoint is accepting writes:

```bash
curl -X POST https://prometheus.fartlab.dev/api/v1/write \
  -H "Content-Type: application/x-protobuf" \
  -d ""
```

- `400 Bad Request` — receiver is enabled (empty body is invalid, but the endpoint exists)
- `404 Not Found` or `405 Method Not Allowed` — receiver is **not** enabled

#### 2. Check VMM logs for export activity

The export thread logs at `info` level. Look for these lines in the VMM output:

| Log message | Meaning |
|-------------|---------|
| `remote-write export thread started` | Thread spawned successfully |
| `remote-write export loop started` | Loop entered, will push after first 10s interval |
| `remote-write: pushed N series (M with data)` | Data sent to Prometheus |
| `remote-write: push failed: ...` | HTTP request failed (check error message) |
| `remote-write: no new data to export` | Drain returned nothing (debug level) |

If none of these appear, the `--remote-write` flag may not have been passed.

#### 3. Check the cursor file

```bash
ssh donald@10.10.11.33 "cat ~/rondo/vmm_metrics/cursor_prometheus.json"
```

If the file exists with position data, the export loop completed at least one successful push. If it doesn't exist, no push succeeded.

#### 4. Check network connectivity from the VMM box

```bash
ssh donald@10.10.11.33 "curl -v https://prometheus.fartlab.dev/api/v1/write"
```

Verify the VMM box can reach Prometheus. TLS errors or timeouts indicate a network/DNS issue.

#### 5. Timing

The export thread sleeps for **10 seconds** before its first push. If the VM shuts down before the first sleep completes, nothing gets pushed. For a 15s workload, only one push cycle may fire. Use `workload_duration=45` or longer to ensure multiple export cycles.

## Updating the Dashboard

1. Edit `deploy/grafana/rondo-vmm-dashboard.json` (the source of truth)
2. Copy the updated JSON into `deploy/k8s/rondo-dashboard-configmap.yaml` under `data.rondo-vmm-dashboard.json`
3. Apply: `kubectl apply -f deploy/k8s/`

Alternatively, edit the dashboard in Grafana's UI, export the JSON, and update both files.

## File Structure

```
deploy/
  grafana/
    rondo-vmm-dashboard.json    # Dashboard JSON (source of truth)
  k8s/
    grafana-dashboard.yaml      # GrafanaDashboard CR
    rondo-dashboard-configmap.yaml  # ConfigMap with dashboard JSON
  README.md                     # This file
```
