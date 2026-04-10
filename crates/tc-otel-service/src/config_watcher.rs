//! File-based configuration watcher with hot-reload support
//!
//! Monitors the configuration file for changes by polling its modification time.
//! When a change is detected, the file is re-read, parsed, and validated. Valid
//! new configurations are broadcast to subscribers via a `tokio::sync::watch` channel.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tc_otel_core::config_watcher::ConfigDiff;
use tc_otel_core::AppSettings;
use tokio::sync::watch;

/// Watches a configuration file and broadcasts validated changes
pub struct ConfigWatcher {
    config_path: PathBuf,
    tx: watch::Sender<AppSettings>,
    poll_interval: Duration,
    last_modified: Option<SystemTime>,
}

impl ConfigWatcher {
    /// Create a new ConfigWatcher.
    ///
    /// Returns the watcher and a receiver that gets updated on config changes.
    pub fn new(
        path: PathBuf,
        initial: AppSettings,
        poll_interval: Duration,
    ) -> (Self, watch::Receiver<AppSettings>) {
        let (tx, rx) = watch::channel(initial);
        let last_modified = file_mtime(&path);
        let watcher = Self {
            config_path: path,
            tx,
            poll_interval,
            last_modified,
        };
        (watcher, rx)
    }

    /// Run the config watcher loop. This blocks until the watch channel is closed.
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.poll_interval);

        loop {
            interval.tick().await;

            let current_mtime = file_mtime(&self.config_path);
            if current_mtime == self.last_modified {
                continue;
            }
            self.last_modified = current_mtime;

            // Debounce: wait briefly for writes to complete
            tokio::time::sleep(Duration::from_millis(100)).await;

            match AppSettings::from_json_file(&self.config_path) {
                Ok(new_settings) => {
                    let diff = ConfigDiff::compute(&self.tx.borrow(), &new_settings);
                    if diff.is_empty() {
                        tracing::debug!("Config file touched but content unchanged");
                        continue;
                    }

                    if diff.has_restart_required_changes() {
                        tracing::warn!(
                            "Configuration changes detected that require restart: \
                             receiver={} service={} outputs={}",
                            diff.receiver_changed,
                            diff.service_changed,
                            diff.outputs_changed,
                        );
                    }

                    if diff.has_hot_reloadable_changes() {
                        tracing::info!(
                            "Hot-reloading configuration: export={} logging={}",
                            diff.export_changed,
                            diff.logging_changed,
                        );
                    }

                    if self.tx.send(new_settings).is_err() {
                        tracing::debug!("Config watch channel closed, stopping watcher");
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to reload config from {}: {}",
                        self.config_path.display(),
                        e
                    );
                }
            }
        }
    }
}

/// Get the modification time of a file, or None if it can't be read
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tc_otel_core::config::*;

    fn default_settings() -> AppSettings {
        AppSettings {
            logging: LoggingConfig {
                log_level: "info".to_string(),
                format: LogFormat::Text,
                output_path: None,
            },
            receiver: ReceiverConfig::default(),
            export: ExportConfig::default(),
            outputs: vec![],
            service: ServiceConfig::default(),
            web: WebConfig::default(),
        }
    }

    fn write_config(path: &Path, settings: &AppSettings) {
        let json = serde_json::to_string_pretty(settings).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(json.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }

    #[tokio::test]
    async fn test_watcher_detects_export_change() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (watcher, mut rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        let handle = tokio::spawn(watcher.run());

        // Wait for watcher to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify config
        let mut updated = initial.clone();
        updated.export.endpoint = "http://new-endpoint:9428/insert".to_string();
        updated.export.batch_size = 5000;
        write_config(&config_path, &updated);

        // Wait for watcher to detect change
        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("timeout waiting for config change")
            .expect("watch channel error");

        let new_config = rx.borrow().clone();
        assert_eq!(
            new_config.export.endpoint,
            "http://new-endpoint:9428/insert"
        );
        assert_eq!(new_config.export.batch_size, 5000);

        handle.abort();
    }

    #[tokio::test]
    async fn test_watcher_detects_logging_change() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (watcher, mut rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        let handle = tokio::spawn(watcher.run());
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut updated = initial.clone();
        updated.logging.log_level = "debug".to_string();
        write_config(&config_path, &updated);

        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("timeout")
            .expect("channel error");

        assert_eq!(rx.borrow().logging.log_level, "debug");
        handle.abort();
    }

    #[tokio::test]
    async fn test_watcher_ignores_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (watcher, mut rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        let handle = tokio::spawn(watcher.run());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Write invalid JSON
        std::fs::write(&config_path, "{ invalid json !!!").unwrap();

        // Wait a bit — should NOT receive a change
        let result = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
        assert!(
            result.is_err(),
            "Should timeout because invalid config is rejected"
        );

        // Original config should still be in place
        assert_eq!(rx.borrow().logging.log_level, "info");

        handle.abort();
    }

    #[tokio::test]
    async fn test_watcher_ignores_unchanged_content() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (watcher, mut rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        let handle = tokio::spawn(watcher.run());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Re-write same content (touches mtime but no semantic change)
        write_config(&config_path, &initial);

        // Should NOT trigger a change notification
        let result = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
        assert!(
            result.is_err(),
            "Should timeout because content is identical"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_watcher_handles_multiple_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (watcher, mut rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        let handle = tokio::spawn(watcher.run());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // First change
        let mut v1 = initial.clone();
        v1.export.batch_size = 1000;
        write_config(&config_path, &v1);

        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("timeout")
            .expect("channel error");
        assert_eq!(rx.borrow().export.batch_size, 1000);

        // Second change
        let mut v2 = v1.clone();
        v2.export.batch_size = 3000;
        write_config(&config_path, &v2);

        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("timeout")
            .expect("channel error");
        assert_eq!(rx.borrow().export.batch_size, 3000);

        handle.abort();
    }

    #[tokio::test]
    async fn test_watcher_initial_config_available() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let initial = default_settings();
        write_config(&config_path, &initial);

        let (_watcher, rx) = ConfigWatcher::new(
            config_path.clone(),
            initial.clone(),
            Duration::from_millis(50),
        );

        // Initial config should be available immediately without any file change
        let current = rx.borrow().clone();
        assert_eq!(current.logging.log_level, "info");
        assert_eq!(current.export.batch_size, 2000);
    }
}
