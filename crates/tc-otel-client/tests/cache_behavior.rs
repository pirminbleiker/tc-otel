//! Behavior tests for `SymbolTreeCache` — insert, get, invalidate, invalidate_all,
//! concurrent reads, and explicit-timestamp insertion.

use chrono::{TimeZone, Utc};
use std::sync::Arc;
use std::thread;
use tc_otel_client::browse::{SymbolNode, SymbolTree};
use tc_otel_client::cache::{SymbolTreeCache, TargetKey};

fn tree_with(names: &[&str]) -> SymbolTree {
    let mut nodes = Vec::with_capacity(names.len());
    for (i, n) in names.iter().enumerate() {
        nodes.push(SymbolNode {
            name: (*n).to_string(),
            type_name: "BOOL".into(),
            comment: String::new(),
            igroup: 0x4040,
            ioffset: i as u32,
            size: 1,
            datatype: 33,
            flags: 0,
        });
    }
    let mut tree = SymbolTree::default();
    // Reconstruct via the public API — build a fresh tree via parse_upload
    // would pull in the parser; cheaper to synthesize one per node here.
    for n in nodes {
        // SymbolTree is mostly the output of parse_upload. To let tests
        // synthesize data without coupling to the parser, expose no mutator
        // — but nodes is public and we build through it. The default tree
        // has empty index_by_name; get() relies on the index, so synthesize
        // by using the public fields only.
        tree.nodes.push(n);
    }
    tree
}

fn tk(a: u8) -> TargetKey {
    TargetKey([a, 0, 0, 0, 1, 1])
}

#[test]
fn insert_then_get_returns_same_len() {
    let cache = SymbolTreeCache::new();
    let t = tree_with(&["A", "B"]);
    cache.insert(tk(1), t);
    let got = cache.get(tk(1)).expect("present");
    assert_eq!(got.nodes.len(), 2);
}

#[test]
fn get_missing_returns_none() {
    let cache = SymbolTreeCache::new();
    assert!(cache.get(tk(1)).is_none());
}

#[test]
fn invalidate_drops_entry() {
    let cache = SymbolTreeCache::new();
    cache.insert(tk(1), tree_with(&["A"]));
    cache.insert(tk(2), tree_with(&["B"]));
    cache.invalidate(tk(1));
    assert!(cache.get(tk(1)).is_none());
    assert!(cache.get(tk(2)).is_some());
    assert_eq!(cache.len(), 1);
}

#[test]
fn invalidate_all_drops_everything() {
    let cache = SymbolTreeCache::new();
    cache.insert(tk(1), tree_with(&["A"]));
    cache.insert(tk(2), tree_with(&["B"]));
    cache.invalidate_all();
    assert!(cache.is_empty());
    assert!(cache.get(tk(1)).is_none());
    assert!(cache.get(tk(2)).is_none());
}

#[test]
fn insert_replaces_existing_entry() {
    let cache = SymbolTreeCache::new();
    cache.insert(tk(1), tree_with(&["A"]));
    cache.insert(tk(1), tree_with(&["A", "B", "C"]));
    assert_eq!(cache.get(tk(1)).unwrap().nodes.len(), 3);
    assert_eq!(cache.len(), 1);
}

#[test]
fn insert_with_time_records_fixed_timestamp() {
    let cache = SymbolTreeCache::new();
    let ts = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    cache.insert_with_time(tk(1), tree_with(&["A"]), ts);
    assert_eq!(cache.fetched_at(tk(1)).unwrap(), ts);
}

#[test]
fn targets_listing_is_complete() {
    let cache = SymbolTreeCache::new();
    cache.insert(tk(1), tree_with(&["A"]));
    cache.insert(tk(2), tree_with(&["B"]));
    let mut targets = cache.targets();
    targets.sort_by_key(|k| k.0[0]);
    assert_eq!(targets, vec![tk(1), tk(2)]);
}

#[test]
fn concurrent_reads_do_not_deadlock() {
    let cache = Arc::new(SymbolTreeCache::new());
    cache.insert(tk(1), tree_with(&["A", "B", "C"]));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let c = cache.clone();
            thread::spawn(move || {
                for _ in 0..1000 {
                    let _ = c.get(tk(1));
                    let _ = c.len();
                    let _ = c.targets();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_reads_while_writer_invalidates() {
    let cache = Arc::new(SymbolTreeCache::new());
    cache.insert(tk(1), tree_with(&["A"]));
    cache.insert(tk(2), tree_with(&["B"]));
    cache.insert(tk(3), tree_with(&["C"]));

    let writer = {
        let c = cache.clone();
        thread::spawn(move || {
            for i in 0..200 {
                let tgt = tk(((i % 3) + 1) as u8);
                c.invalidate(tgt);
                c.insert(tgt, tree_with(&["re-inserted"]));
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let c = cache.clone();
            thread::spawn(move || {
                for _ in 0..500 {
                    for i in 1..=3u8 {
                        let _ = c.get(tk(i));
                    }
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    // Cache should still be consistent (no panic) and nonempty.
    assert!(!cache.is_empty());
}

#[test]
fn ams_netid_conversion_roundtrips() {
    let src = ads::AmsNetId::new(10, 20, 30, 40, 1, 1);
    let key: TargetKey = src.into();
    assert_eq!(key, TargetKey([10, 20, 30, 40, 1, 1]));
    assert_eq!(format!("{key}"), "10.20.30.40.1.1");
}
