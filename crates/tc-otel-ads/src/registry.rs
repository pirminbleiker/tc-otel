//! Task registry for tracking registration metadata (protocol v2)

use crate::protocol::{RegistrationKey, TaskMetadata};
use std::collections::HashMap;
use std::sync::RwLock;

/// Thread-safe registry for task metadata indexed by (AMS Net ID, Source Port, Task Index)
pub struct TaskRegistry {
    metadata: RwLock<HashMap<RegistrationKey, TaskMetadata>>,
}

impl TaskRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            metadata: RwLock::new(HashMap::new()),
        }
    }

    /// Register or update task metadata
    pub fn register(&self, key: RegistrationKey, metadata: TaskMetadata) {
        self.metadata.write().unwrap().insert(key, metadata);
    }

    /// Look up task metadata by key
    pub fn lookup(&self, key: &RegistrationKey) -> Option<TaskMetadata> {
        self.metadata.read().unwrap().get(key).cloned()
    }

    /// Partial lookup — matches the first entry whose key satisfies
    /// every supplied component. `None` filters skip that field.
    /// Used by metric paths that know only a subset of the
    /// `(net_id, ams_source_port, task_index)` triple:
    /// - `PlcSystemMetricsCollector` / `metric_aggregate_to_entries`
    ///   know `(net_id, task_index)` but not `ams_source_port`.
    /// - `batch_to_metrics` knows `(net_id, ams_source_port)` but not
    ///   `task_index` (zero-default on the MetricEntry).
    ///
    /// Returns the matching key so callers can backfill the missing
    /// component(s). One PLC task = one ams_source_port = one
    /// task_index, so ambiguity is practically impossible.
    pub fn lookup_partial(
        &self,
        ams_net_id: &str,
        ams_source_port: Option<u16>,
        task_index: Option<u8>,
    ) -> Option<(RegistrationKey, TaskMetadata)> {
        self.metadata
            .read()
            .unwrap()
            .iter()
            .find(|(k, _)| {
                k.ams_net_id == ams_net_id
                    && ams_source_port.is_none_or(|p| k.ams_source_port == p)
                    && task_index.is_none_or(|t| k.task_index == t)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    /// Convenience wrapper for `lookup_partial` when only the task
    /// index is known (the most common case from the metric-side
    /// backfill path).
    pub fn lookup_by_task(
        &self,
        ams_net_id: &str,
        task_index: u8,
    ) -> Option<(RegistrationKey, TaskMetadata)> {
        self.lookup_partial(ams_net_id, None, Some(task_index))
    }

    /// Get the number of registered tasks
    pub fn len(&self) -> usize {
        self.metadata.read().unwrap().len()
    }

    /// Check if registry is empty
    pub fn is_empty(&self) -> bool {
        self.metadata.read().unwrap().is_empty()
    }

    /// Get all registered tasks as a snapshot
    pub fn all_tasks(&self) -> Vec<(RegistrationKey, TaskMetadata)> {
        self.metadata
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Clear all registrations
    pub fn clear(&self) {
        self.metadata.write().unwrap().clear();
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_lookup() {
        let registry = TaskRegistry::new();
        let key = RegistrationKey {
            ams_net_id: "5.80.201.232.1.1".to_string(),
            ams_source_port: 851,
            task_index: 0,
        };
        let metadata = TaskMetadata {
            task_name: "MyTask".to_string(),
            app_name: "MyApp".to_string(),
            project_name: "MyProject".to_string(),
            online_change_count: 42,
            app_port: 851,
        };

        registry.register(key.clone(), metadata.clone());
        let found = registry.lookup(&key);

        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.task_name, "MyTask");
        assert_eq!(found.app_name, "MyApp");
        assert_eq!(found.project_name, "MyProject");
        assert_eq!(found.online_change_count, 42);
    }

    #[test]
    fn test_update_existing() {
        let registry = TaskRegistry::new();
        let key = RegistrationKey {
            ams_net_id: "5.80.201.232.1.1".to_string(),
            ams_source_port: 851,
            task_index: 1,
        };
        let metadata1 = TaskMetadata {
            task_name: "Task1".to_string(),
            app_name: "App1".to_string(),
            project_name: "Project1".to_string(),
            online_change_count: 1,
            app_port: 851,
        };
        let metadata2 = TaskMetadata {
            task_name: "Task1".to_string(),
            app_name: "App1".to_string(),
            project_name: "Project1".to_string(),
            online_change_count: 2,
            app_port: 851,
        };

        registry.register(key.clone(), metadata1);
        registry.register(key.clone(), metadata2);

        let found = registry.lookup(&key).unwrap();
        assert_eq!(found.online_change_count, 2);
    }

    #[test]
    fn test_lookup_nonexistent() {
        let registry = TaskRegistry::new();
        let key = RegistrationKey {
            ams_net_id: "5.80.201.232.1.1".to_string(),
            ams_source_port: 851,
            task_index: 99,
        };

        assert!(registry.lookup(&key).is_none());
    }

    #[test]
    fn test_len_and_clear() {
        let registry = TaskRegistry::new();
        let key1 = RegistrationKey {
            ams_net_id: "5.80.201.232.1.1".to_string(),
            ams_source_port: 851,
            task_index: 0,
        };
        let key2 = RegistrationKey {
            ams_net_id: "5.80.201.232.1.1".to_string(),
            ams_source_port: 851,
            task_index: 1,
        };
        let metadata = TaskMetadata {
            task_name: "Task".to_string(),
            app_name: "App".to_string(),
            project_name: "Project".to_string(),
            online_change_count: 0,
            app_port: 851,
        };

        registry.register(key1, metadata.clone());
        registry.register(key2, metadata);

        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());

        registry.clear();
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }
}
