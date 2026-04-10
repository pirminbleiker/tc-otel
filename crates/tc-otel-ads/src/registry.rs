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
        };
        let metadata2 = TaskMetadata {
            task_name: "Task1".to_string(),
            app_name: "App1".to_string(),
            project_name: "Project1".to_string(),
            online_change_count: 2,
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
    fn test_all_tasks() {
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
        let metadata1 = TaskMetadata {
            task_name: "Task1".to_string(),
            app_name: "App".to_string(),
            project_name: "Project".to_string(),
            online_change_count: 0,
        };
        let metadata2 = TaskMetadata {
            task_name: "Task2".to_string(),
            app_name: "App".to_string(),
            project_name: "Project".to_string(),
            online_change_count: 0,
        };

        registry.register(key1, metadata1);
        registry.register(key2, metadata2);

        let all = registry.all_tasks();
        assert_eq!(all.len(), 2);
        let names: Vec<_> = all.iter().map(|(_, m)| m.task_name.as_str()).collect();
        assert!(names.contains(&"Task1"));
        assert!(names.contains(&"Task2"));
    }

    #[test]
    fn test_all_tasks_empty() {
        let registry = TaskRegistry::new();
        let all = registry.all_tasks();
        assert!(all.is_empty());
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
