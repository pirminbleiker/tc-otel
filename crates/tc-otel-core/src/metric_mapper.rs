//! Maps PLC symbols to OTEL metric definitions via configuration.
//!
//! The `MetricMapper` applies custom metric definitions from the config to
//! incoming `MetricEntry` values. When a metric has a `plc.symbol` attribute
//! matching a configured symbol, the mapper overwrites the metric's name,
//! description, unit, and kind from the config definition.

use std::collections::HashMap;

use crate::config::{CustomMetricDef, MetricsConfig};
use crate::models::MetricEntry;

/// Applies configured PLC symbol → OTEL metric mappings to MetricEntry values.
#[derive(Debug, Clone)]
pub struct MetricMapper {
    /// Symbol → definition lookup (O(1) per apply)
    defs: HashMap<String, CustomMetricDef>,
}

impl MetricMapper {
    /// Build a mapper from the metrics config section.
    pub fn from_config(config: &MetricsConfig) -> Self {
        let mut defs = HashMap::with_capacity(config.custom_metrics.len());
        for def in &config.custom_metrics {
            defs.insert(def.symbol.clone(), def.clone());
        }
        Self { defs }
    }

    /// Number of configured symbol mappings.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether the mapper has no configured mappings.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Apply the mapping to a MetricEntry. If the entry has a `plc.symbol`
    /// attribute matching a configured definition, the entry's name,
    /// description, unit, kind, and is_monotonic are overwritten.
    ///
    /// Returns `true` if a mapping was applied, `false` otherwise.
    pub fn apply(&self, entry: &mut MetricEntry) -> bool {
        if self.defs.is_empty() {
            return false;
        }

        let symbol = match entry.attributes.get("plc.symbol") {
            Some(serde_json::Value::String(s)) => s.as_str(),
            _ => return false,
        };

        let def = match self.defs.get(symbol) {
            Some(d) => d,
            None => return false,
        };

        entry.name = def.metric_name.clone();
        entry.description = def.description.clone();
        entry.unit = def.unit.clone();
        entry.kind = def.kind.to_metric_kind();
        entry.is_monotonic = def.is_monotonic;

        true
    }

    /// Validate a list of custom metric definitions.
    /// Returns a list of error messages (empty if valid).
    pub fn validate(defs: &[CustomMetricDef]) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen_symbols = HashMap::with_capacity(defs.len());

        for (i, def) in defs.iter().enumerate() {
            if def.symbol.is_empty() {
                errors.push(format!(
                    "custom_metrics[{}]: symbol must not be empty",
                    i
                ));
            }
            if def.metric_name.is_empty() {
                errors.push(format!(
                    "custom_metrics[{}]: metric_name must not be empty",
                    i
                ));
            }
            if let Some(prev_idx) = seen_symbols.insert(&def.symbol, i) {
                if !def.symbol.is_empty() {
                    errors.push(format!(
                        "custom_metrics[{}]: duplicate symbol '{}' (first at [{}])",
                        i, def.symbol, prev_idx
                    ));
                }
            }
        }

        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetricKindConfig;

    #[test]
    fn test_empty_mapper() {
        let mapper = MetricMapper::from_config(&MetricsConfig::default());
        assert!(mapper.is_empty());
        assert_eq!(mapper.len(), 0);
    }

    #[test]
    fn test_apply_returns_false_when_empty() {
        let mapper = MetricMapper::from_config(&MetricsConfig::default());
        let mut entry = MetricEntry::gauge("test".to_string(), 1.0);
        assert!(!mapper.apply(&mut entry));
    }

    #[test]
    fn test_apply_matches_symbol() {
        let config = MetricsConfig {
            custom_metrics: vec![CustomMetricDef {
                symbol: "GVL.temp".to_string(),
                metric_name: "plc.temperature".to_string(),
                description: "Temperature".to_string(),
                unit: "Cel".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
            }],
            ..MetricsConfig::default()
        };
        let mapper = MetricMapper::from_config(&config);

        let mut entry = MetricEntry::gauge("raw".to_string(), 25.0);
        entry
            .attributes
            .insert("plc.symbol".to_string(), serde_json::json!("GVL.temp"));

        assert!(mapper.apply(&mut entry));
        assert_eq!(entry.name, "plc.temperature");
        assert_eq!(entry.unit, "Cel");
    }

    #[test]
    fn test_apply_no_match() {
        let config = MetricsConfig {
            custom_metrics: vec![CustomMetricDef {
                symbol: "GVL.temp".to_string(),
                metric_name: "plc.temperature".to_string(),
                description: "".to_string(),
                unit: "".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
            }],
            ..MetricsConfig::default()
        };
        let mapper = MetricMapper::from_config(&config);

        let mut entry = MetricEntry::gauge("raw".to_string(), 25.0);
        entry
            .attributes
            .insert("plc.symbol".to_string(), serde_json::json!("GVL.other"));

        assert!(!mapper.apply(&mut entry));
        assert_eq!(entry.name, "raw");
    }

    #[test]
    fn test_validate_empty() {
        assert!(MetricMapper::validate(&[]).is_empty());
    }

    #[test]
    fn test_validate_duplicate() {
        let defs = vec![
            CustomMetricDef {
                symbol: "GVL.x".to_string(),
                metric_name: "a".to_string(),
                description: "".to_string(),
                unit: "".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
            },
            CustomMetricDef {
                symbol: "GVL.x".to_string(),
                metric_name: "b".to_string(),
                description: "".to_string(),
                unit: "".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
            },
        ];
        let errs = MetricMapper::validate(&defs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("duplicate"));
    }
}
