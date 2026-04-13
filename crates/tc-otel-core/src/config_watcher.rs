//! Configuration diffing and hot-reload support
//!
//! Computes which configuration sections changed between two `AppSettings`
//! instances, enabling hot-reload of supported settings without service restart.

use crate::config::AppSettings;

/// Identifies which configuration sections changed between two settings
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDiff {
    pub export_changed: bool,
    pub logging_changed: bool,
    pub receiver_changed: bool,
    pub service_changed: bool,
    pub outputs_changed: bool,
    pub web_changed: bool,
}

impl ConfigDiff {
    /// Compute the diff between two configurations
    pub fn compute(old: &AppSettings, new: &AppSettings) -> Self {
        Self {
            export_changed: old.export != new.export,
            logging_changed: old.logging != new.logging,
            receiver_changed: old.receiver != new.receiver,
            service_changed: old.service != new.service,
            outputs_changed: old.outputs != new.outputs,
            web_changed: old.web != new.web,
        }
    }

    /// Returns true if any hot-reloadable settings changed (export, logging)
    pub fn has_hot_reloadable_changes(&self) -> bool {
        self.export_changed || self.logging_changed
    }

    /// Returns true if any settings that require restart changed
    pub fn has_restart_required_changes(&self) -> bool {
        self.receiver_changed || self.service_changed || self.outputs_changed || self.web_changed
    }

    /// Returns true if nothing changed
    pub fn is_empty(&self) -> bool {
        !self.export_changed
            && !self.logging_changed
            && !self.receiver_changed
            && !self.service_changed
            && !self.outputs_changed
            && !self.web_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

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
            metrics: MetricsConfig::default(),
        }
    }

    #[test]
    fn test_identical_configs_produce_empty_diff() {
        let a = default_settings();
        let b = default_settings();
        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.is_empty());
        assert!(!diff.has_hot_reloadable_changes());
        assert!(!diff.has_restart_required_changes());
    }

    #[test]
    fn test_export_endpoint_change_detected() {
        let a = default_settings();
        let mut b = default_settings();
        b.export.endpoint = "http://new-endpoint:9428/insert".to_string();

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.export_changed);
        assert!(!diff.logging_changed);
        assert!(!diff.receiver_changed);
        assert!(diff.has_hot_reloadable_changes());
        assert!(!diff.has_restart_required_changes());
    }

    #[test]
    fn test_export_batch_size_change_detected() {
        let a = default_settings();
        let mut b = default_settings();
        b.export.batch_size = 5000;

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.export_changed);
        assert!(diff.has_hot_reloadable_changes());
    }

    #[test]
    fn test_logging_level_change_detected() {
        let a = default_settings();
        let mut b = default_settings();
        b.logging.log_level = "debug".to_string();

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.logging_changed);
        assert!(diff.has_hot_reloadable_changes());
        assert!(!diff.has_restart_required_changes());
    }

    #[test]
    fn test_logging_format_change_detected() {
        let a = default_settings();
        let mut b = default_settings();
        b.logging.format = LogFormat::Json;

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.logging_changed);
    }

    #[test]
    fn test_receiver_change_requires_restart() {
        let a = default_settings();
        let mut b = default_settings();
        b.receiver.http_port = 9999;

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.receiver_changed);
        assert!(!diff.has_hot_reloadable_changes());
        assert!(diff.has_restart_required_changes());
    }

    #[test]
    fn test_service_change_requires_restart() {
        let a = default_settings();
        let mut b = default_settings();
        b.service.channel_capacity = 100_000;

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.service_changed);
        assert!(diff.has_restart_required_changes());
    }

    #[test]
    fn test_outputs_change_requires_restart() {
        let a = default_settings();
        let mut b = default_settings();
        b.outputs.push(OutputConfig {
            output_type: "console".to_string(),
            settings: serde_json::json!({}),
        });

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.outputs_changed);
        assert!(diff.has_restart_required_changes());
    }

    #[test]
    fn test_multiple_sections_changed() {
        let a = default_settings();
        let mut b = default_settings();
        b.export.endpoint = "http://new:9428".to_string();
        b.logging.log_level = "debug".to_string();
        b.receiver.http_port = 8080;

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.export_changed);
        assert!(diff.logging_changed);
        assert!(diff.receiver_changed);
        assert!(!diff.service_changed);
        assert!(!diff.outputs_changed);
        assert!(diff.has_hot_reloadable_changes());
        assert!(diff.has_restart_required_changes());
    }

    #[test]
    fn test_diff_is_not_empty_when_changes_exist() {
        let a = default_settings();
        let mut b = default_settings();
        b.export.batch_size = 1;

        let diff = ConfigDiff::compute(&a, &b);
        assert!(!diff.is_empty());
    }
}
