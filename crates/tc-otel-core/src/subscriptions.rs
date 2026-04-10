//! Subscription manager for PLC tag variable selection
//!
//! Users browse tags in the web UI and subscribe to up to `max_subscriptions`
//! variables for real-time metric collection.

use serde::Serialize;
use std::collections::HashSet;
use std::sync::Mutex;

/// Manages tag subscriptions with an enforced maximum count
#[derive(Debug)]
pub struct SubscriptionManager {
    max_subscriptions: usize,
    subscriptions: Mutex<HashSet<String>>,
}

/// Error when subscription operations fail
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SubscriptionError {
    /// Subscription limit reached
    LimitReached { max: usize, current: usize },
    /// Tag not found in subscriptions
    NotFound { tag: String },
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriptionError::LimitReached { max, current } => {
                write!(
                    f,
                    "subscription limit reached: {current}/{max} subscriptions active"
                )
            }
            SubscriptionError::NotFound { tag } => {
                write!(f, "tag '{}' not found in subscriptions", tag)
            }
        }
    }
}

impl SubscriptionManager {
    /// Create a new SubscriptionManager with the given limit
    pub fn new(max_subscriptions: usize) -> Self {
        Self {
            max_subscriptions,
            subscriptions: Mutex::new(HashSet::new()),
        }
    }

    /// Subscribe to a tag. Returns error if limit reached.
    pub fn subscribe(&self, tag: String) -> Result<(), SubscriptionError> {
        let mut subs = self.subscriptions.lock().unwrap();
        if subs.contains(&tag) {
            return Ok(()); // already subscribed
        }
        if subs.len() >= self.max_subscriptions {
            return Err(SubscriptionError::LimitReached {
                max: self.max_subscriptions,
                current: subs.len(),
            });
        }
        subs.insert(tag);
        Ok(())
    }

    /// Unsubscribe from a tag
    pub fn unsubscribe(&self, tag: &str) -> Result<(), SubscriptionError> {
        let mut subs = self.subscriptions.lock().unwrap();
        if !subs.remove(tag) {
            return Err(SubscriptionError::NotFound {
                tag: tag.to_string(),
            });
        }
        Ok(())
    }

    /// List all current subscriptions
    pub fn list(&self) -> Vec<String> {
        let subs = self.subscriptions.lock().unwrap();
        let mut tags: Vec<_> = subs.iter().cloned().collect();
        tags.sort();
        tags
    }

    /// Get subscription count
    pub fn count(&self) -> usize {
        self.subscriptions.lock().unwrap().len()
    }

    /// Get the maximum allowed subscriptions
    pub fn max(&self) -> usize {
        self.max_subscriptions
    }

    /// Clear all subscriptions
    pub fn clear(&self) {
        self.subscriptions.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscribe_and_list() {
        let mgr = SubscriptionManager::new(500);
        mgr.subscribe("GVL.sensor_temp".to_string()).unwrap();
        mgr.subscribe("GVL.motor_speed".to_string()).unwrap();

        let list = mgr.list();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"GVL.sensor_temp".to_string()));
        assert!(list.contains(&"GVL.motor_speed".to_string()));
    }

    #[test]
    fn test_subscribe_duplicate_is_ok() {
        let mgr = SubscriptionManager::new(500);
        mgr.subscribe("GVL.temp".to_string()).unwrap();
        mgr.subscribe("GVL.temp".to_string()).unwrap();
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn test_subscribe_limit_enforced() {
        let mgr = SubscriptionManager::new(3);
        mgr.subscribe("tag1".to_string()).unwrap();
        mgr.subscribe("tag2".to_string()).unwrap();
        mgr.subscribe("tag3".to_string()).unwrap();

        let result = mgr.subscribe("tag4".to_string());
        assert!(matches!(
            result,
            Err(SubscriptionError::LimitReached { max: 3, current: 3 })
        ));
    }

    #[test]
    fn test_unsubscribe() {
        let mgr = SubscriptionManager::new(500);
        mgr.subscribe("tag1".to_string()).unwrap();
        mgr.subscribe("tag2".to_string()).unwrap();

        mgr.unsubscribe("tag1").unwrap();
        assert_eq!(mgr.count(), 1);

        let list = mgr.list();
        assert!(!list.contains(&"tag1".to_string()));
        assert!(list.contains(&"tag2".to_string()));
    }

    #[test]
    fn test_unsubscribe_not_found() {
        let mgr = SubscriptionManager::new(500);
        let result = mgr.unsubscribe("nonexistent");
        assert!(matches!(result, Err(SubscriptionError::NotFound { .. })));
    }

    #[test]
    fn test_clear() {
        let mgr = SubscriptionManager::new(500);
        mgr.subscribe("tag1".to_string()).unwrap();
        mgr.subscribe("tag2".to_string()).unwrap();
        mgr.clear();
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn test_max() {
        let mgr = SubscriptionManager::new(500);
        assert_eq!(mgr.max(), 500);
    }

    #[test]
    fn test_subscribe_after_unsubscribe_within_limit() {
        let mgr = SubscriptionManager::new(2);
        mgr.subscribe("tag1".to_string()).unwrap();
        mgr.subscribe("tag2".to_string()).unwrap();

        // At limit
        assert!(mgr.subscribe("tag3".to_string()).is_err());

        // Free a slot
        mgr.unsubscribe("tag1").unwrap();

        // Now should succeed
        mgr.subscribe("tag3".to_string()).unwrap();
        assert_eq!(mgr.count(), 2);
    }

    #[test]
    fn test_subscription_error_display() {
        let err = SubscriptionError::LimitReached {
            max: 500,
            current: 500,
        };
        assert!(err.to_string().contains("500"));

        let err = SubscriptionError::NotFound {
            tag: "foo".to_string(),
        };
        assert!(err.to_string().contains("foo"));
    }
}
