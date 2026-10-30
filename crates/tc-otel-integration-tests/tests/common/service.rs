//! Starts the real `TcOtelService` in-task on ephemeral loopback ports.

use std::net::SocketAddr;
use std::time::Duration;

use tc_otel_core::config::{TcpTransportConfig, TransportConfig, WireFormat};
use tc_otel_core::AppSettings;
use tc_otel_service::service::TcOtelService;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Which signals and wire format the service should export.
pub struct ServiceOptions {
    pub logs_endpoint: String,
    pub traces_endpoint: Option<String>,
    pub format: WireFormat,
}

pub struct TestService {
    ams_addr: SocketAddr,
    task: JoinHandle<()>,
}

/// Reserve a free loopback port. The listener is dropped before the service
/// binds it, so a collision is possible but unlikely; `start` fails loudly.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a free port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn settings(opts: &ServiceOptions, ams_port: u16) -> AppSettings {
    let mut s: AppSettings = toml::from_str(
        r#"
[logging]
log_level = "warn"
format = "json"

[receiver]
host = "127.0.0.1"
http_port = 0
grpc_port = 0
max_body_size = 4194304
request_timeout_secs = 30

[service]
name = "TcOtelService"
display_name = "TC-OTel e2e"
worker_threads = 2
channel_capacity = 1000
shutdown_timeout_secs = 1

[[outputs]]
Type = "otel"
"#,
    )
    .expect("base settings parse");

    s.receiver.ams_tcp_port = ams_port;
    s.receiver.transport = TransportConfig::Tcp(TcpTransportConfig {
        host: "127.0.0.1".to_string(),
        port: ams_port,
        max_connections: 100,
    });
    // Tests open several connections from 127.0.0.1 back to back.
    s.receiver.max_connections_per_ip = 100;
    s.receiver.rate_limit_per_sec_per_ip = 1000;

    s.web.enabled = false;
    s.metrics.export_enabled = false;
    s.metrics.cycle_time_enabled = false;

    s.export.endpoint = opts.logs_endpoint.clone();
    s.export.format = opts.format;
    s.export.batch_size = 1;
    s.export.flush_interval_ms = 50;
    s.export.max_retries = 0;
    s.export.timeout_secs = 5;

    if let Some(endpoint) = &opts.traces_endpoint {
        s.traces.enabled = true;
        s.traces.export.endpoint = Some(endpoint.clone());
        s.traces.export.batch_size = 1;
        s.traces.export.flush_interval_ms = 50;
    }
    s
}

impl TestService {
    /// Start the service and wait until its AMS/TCP listener accepts.
    pub async fn start(opts: ServiceOptions) -> TestService {
        let port = free_port();
        let service = TcOtelService::new(settings(&opts, port))
            .await
            .expect("create service");
        // `run` blocks on ctrl-c, so stopping it means aborting the task.
        let task = tokio::spawn(async move {
            let _ = service.run().await;
        });
        let ams_addr: SocketAddr = ([127, 0, 0, 1], port).into();

        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if TcpStream::connect(ams_addr).await.is_ok() {
                break;
            }
            assert!(
                !task.is_finished(),
                "service exited before its AMS/TCP listener came up"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "service AMS/TCP listener not ready on {ams_addr}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        TestService { ams_addr, task }
    }

    /// Bound AMS/TCP address the fake PLCs connect to.
    pub fn ams_addr(&self) -> SocketAddr {
        self.ams_addr
    }

    /// True while the service task is still running.
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for TestService {
    fn drop(&mut self) {
        self.task.abort();
    }
}
