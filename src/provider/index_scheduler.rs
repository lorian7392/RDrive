//! Metadata-only directory work selection. The caller performs and durably
//! records one successful or failed Depth:1 attempt; this module does no I/O.

use super::{explicit_pin, CatalogDemand, Journal};
use crate::config::IndexPolicy;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
struct Candidate {
    id: String,
    depth: usize,
    last: Option<i64>,
    attempted: Option<i64>,
    explicit_cold: bool,
    forced: bool,
    active: bool,
    tie: usize,
}

impl Candidate {
    fn priority(&self) -> u8 {
        if self.explicit_cold {
            0
        } else if self.id == "root" && self.last.is_none() {
            1
        } else if self.forced {
            2
        } else if self.id == "root" {
            3
        } else if self.last.is_none() {
            4
        } else if self.active {
            5
        } else {
            6
        }
    }

    fn oldest_attempt(&self) -> i64 {
        self.attempted.or(self.last).unwrap_or(i64::MIN)
    }
}

fn millis(seconds: u64) -> i64 {
    seconds.saturating_mul(1_000).min(i64::MAX as u64) as i64
}

fn valid_directory_demand(j: &Journal, demand: &CatalogDemand) -> bool {
    !demand.metadata
        && !j
            .binding
            .as_ref()
            .is_some_and(|binding| binding != &demand.binding)
        && if demand.id == "root" {
            demand.path.is_empty()
        } else {
            j.items
                .get(&demand.id)
                .is_some_and(|record| record.item.kind == "directory" && record.path == demand.path)
        }
}

fn completed(j: &Journal, demand: &CatalogDemand) -> bool {
    (demand.force || demand.recursive)
        && j.completed_demands
            .get(&demand.id)
            .is_some_and(|requested| *requested >= demand.requested)
}

fn contains_path(parent: &str, child: &str) -> bool {
    parent.is_empty()
        || parent == child
        || child
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Identify fully consumed one-shot requests for durable completion markers.
/// Retaining a marker instead of unlinking a hint cannot delete a newer CLI
/// request that raced publication. A later request timestamp rearms the scope.
pub(super) fn completed_manual_demands(
    j: &Journal,
    demands: &[CatalogDemand],
    repository: bool,
) -> Vec<(String, i64)> {
    let mut finished = BTreeMap::<String, i64>::new();
    for demand in demands
        .iter()
        .filter(|demand| demand.force && valid_directory_demand(j, demand) && !completed(j, demand))
    {
        let successful = |id: &str| {
            j.directory_scans.get(id).is_some_and(|scanned| {
                *scanned >= demand.requested
                    && !j
                        .directory_attempts
                        .get(id)
                        .is_some_and(|attempt| attempt.failures > 0 && attempt.last >= *scanned)
            })
        };
        let done = if repository {
            // A committed manifest snapshot supplies the complete namespace.
            successful("root")
        } else {
            successful(&demand.id)
                && (!demand.recursive
                    || j.items.iter().all(|(id, record)| {
                        record.item.kind != "directory"
                            || !contains_path(&demand.path, &record.path)
                            || successful(id)
                    }))
        };
        if done {
            finished
                .entry(demand.id.clone())
                .and_modify(|requested| *requested = (*requested).max(demand.requested))
                .or_insert(demand.requested);
        }
    }
    finished.into_iter().collect()
}

/// Choose at most one metadata listing, with `now` in Unix milliseconds.
///
/// Root depth is zero; listing a depth-N directory discovers its direct
/// children. Background preindex follows known directories in breadth-first
/// order. Explicit browsing, one-shot recursive demands and inherited pins can
/// cross the configured prefetch boundary. `working_set` is never a directory
/// demand and cannot trigger a recursive network scan.
///
/// Successful scan times enforce the active/general TTL. A forced demand can
/// bypass that TTL once, but no request bypasses an attempt's retry backoff.
/// Two normal priority turns alternate with a fair refresh turn and a crawl
/// turn, so repeated browsing/failures cannot consume all background work.
/// The caller increments `directory_turn` when it records an actual attempt.
#[cfg(test)]
pub(super) fn next_directory(
    j: &Journal,
    demands: &[CatalogDemand],
    policy: &IndexPolicy,
    now: i64,
) -> Option<String> {
    next_directory_where(j, demands, policy, now, |_, _| true)
}

pub(super) fn next_directory_where(
    j: &Journal,
    demands: &[CatalogDemand],
    policy: &IndexPolicy,
    now: i64,
    allowed: impl Fn(&str, bool) -> bool,
) -> Option<String> {
    let demands = demands
        .iter()
        .filter(|demand| valid_directory_demand(j, demand) && !completed(j, demand));
    let mut direct_demands: BTreeMap<&str, Vec<&CatalogDemand>> = BTreeMap::new();
    let mut recursive_demands: BTreeMap<&str, Vec<&CatalogDemand>> = BTreeMap::new();
    for demand in demands {
        direct_demands.entry(&demand.id).or_default().push(demand);
        if demand.recursive {
            recursive_demands
                .entry(&demand.path)
                .or_default()
                .push(demand);
        }
    }
    // A pinned file needs its parent refreshed to discover a replacement even
    // when no general crawl is enabled. A directory pin additionally protects
    // the entire descendant namespace through explicit_pin below.
    let pinned_file_parents: BTreeSet<_> = j
        .items
        .iter()
        .filter_map(|(id, record)| {
            (record.item.kind == "file"
                && allowed(&record.path, false)
                && explicit_pin(j, id, &j.items))
            .then_some(record.item.parent_id.as_str())
        })
        .collect();

    let mut directories = vec![("root", "")];
    directories.extend(j.items.iter().filter_map(|(id, record)| {
        (id != "root" && record.item.kind == "directory")
            .then_some((id.as_str(), record.path.as_str()))
    }));
    let mut candidates = Vec::new();
    for (id, path) in directories {
        if id != "root" && !allowed(path, true) {
            continue;
        }
        if j.directory_attempts
            .get(id)
            .is_some_and(|attempt| attempt.next_try > now)
        {
            continue;
        }
        let depth = if id == "root" {
            0
        } else {
            path.split('/').count()
        };
        let last = j.directory_scans.get(id).copied();
        let background = id == "root"
            || (policy.enabled
                && policy
                    .prefetch_depth
                    .is_none_or(|limit| depth <= limit as usize));
        let pinned = explicit_pin(j, id, &j.items) || pinned_file_parents.contains(id);
        let mut browsed = false;
        let mut requested = false;
        let mut manual_pending = false;
        let mut forced = false;
        let mut matching = direct_demands
            .get(id)
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        // Looking up exact ancestor paths avoids comparing every ordinary
        // browse demand with every directory in a large account. Ancestors
        // are slash-delimited, so a request for "a" never includes "ab".
        let mut ancestor = path;
        loop {
            if let Some(requests) = recursive_demands.get(ancestor) {
                matching.extend(requests.iter().copied());
            }
            if ancestor.is_empty() {
                break;
            }
            ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
        for demand in matching {
            let exact = demand.id == id;
            let failed_after_success = (demand.force || demand.recursive)
                && last.is_some_and(|last| {
                    j.directory_attempts
                        .get(id)
                        .is_some_and(|attempt| attempt.failures > 0 && attempt.last >= last)
                });
            let pending = last.is_none_or(|last| demand.requested > last) || failed_after_success;
            browsed |= exact && !demand.force && !demand.recursive;
            requested |= pending;
            manual_pending |= pending && (demand.force || demand.recursive);
            forced |= pending && demand.force;
        }
        if !background && !pinned && !browsed && !manual_pending {
            continue;
        }
        let active = pinned || browsed || manual_pending;
        let ttl = millis(if active {
            policy.active_refresh_secs
        } else {
            policy.refresh_secs
        });
        if !forced && last.is_some_and(|last| now.saturating_sub(last) < ttl) {
            continue;
        }
        candidates.push(Candidate {
            id: id.into(),
            depth,
            last,
            attempted: j.directory_attempts.get(id).map(|attempt| attempt.last),
            explicit_cold: last.is_none() && requested,
            forced,
            active,
            tie: 0,
        });
    }
    let count = candidates.len();
    if count == 0 {
        return None;
    }
    // Rotate exact ties, including journals whose scans share a timestamp.
    let rotation = (j.directory_turn % count as u64) as usize;
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.tie = (index + count - rotation) % count;
    }

    // A cold root establishes the namespace before background work. An explicit
    // never-seen user scope still has the documented higher normal priority.
    let cold_root = candidates
        .iter()
        .any(|candidate| candidate.id == "root" && candidate.last.is_none());
    let selected = if cold_root {
        None
    } else {
        match j.directory_turn % 4 {
            2 => candidates
                .iter()
                .filter(|candidate| candidate.last.is_some())
                .min_by_key(|candidate| {
                    (
                        candidate.oldest_attempt(),
                        candidate.last,
                        candidate.priority(),
                        candidate.tie,
                    )
                }),
            3 => candidates
                .iter()
                .filter(|candidate| candidate.last.is_none())
                .min_by_key(|candidate| {
                    (
                        candidate.attempted.is_some(),
                        candidate.depth,
                        candidate.oldest_attempt(),
                        candidate.tie,
                    )
                }),
            _ => None,
        }
    };
    selected
        .or_else(|| {
            candidates.iter().min_by_key(|candidate| {
                (
                    candidate.priority(),
                    if candidate.last.is_none() {
                        candidate.depth
                    } else {
                        0
                    },
                    candidate.oldest_attempt(),
                    candidate.last,
                    candidate.tie,
                )
            })
        })
        .map(|candidate| candidate.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{root_item, Domain, Record, ScanAttempt};

    const NOW: i64 = 1_000_000;

    fn policy() -> IndexPolicy {
        IndexPolicy {
            enabled: true,
            prefetch_depth: None,
            refresh_secs: 300,
            active_refresh_secs: 30,
        }
    }

    fn journal() -> Journal {
        Journal {
            binding: Some("binding".into()),
            ..Journal::default()
        }
    }

    fn directory(j: &mut Journal, id: &str, path: &str, parent: &str) {
        let domain = Domain {
            id: "test".into(),
            profile: "test".into(),
            remote: String::new(),
            display_name: "test".into(),
            repository: false,
            local_root: None,
            smart: true,
        };
        let mut item = root_item(&domain);
        item.id = id.into();
        item.parent_id = parent.into();
        item.name = path.rsplit('/').next().unwrap_or(path).into();
        j.items.insert(
            id.into(),
            Record {
                path: path.into(),
                item,
                entry: None,
                strong: None,
                client_id: None,
                metadata_known: false,
                metadata_checked: 0,
            },
        );
    }

    fn demand(id: &str, path: &str, requested: i64) -> CatalogDemand {
        CatalogDemand {
            binding: "binding".into(),
            id: id.into(),
            path: path.into(),
            metadata: false,
            requested,
            recursive: false,
            force: false,
        }
    }

    fn scanned(j: &mut Journal, id: &str, now: i64) {
        j.directory_scans.insert(id.into(), now);
        j.directory_attempts.insert(
            id.into(),
            ScanAttempt {
                last: now,
                failures: 0,
                next_try: now + 3_000,
            },
        );
        j.directory_turn += 1;
    }

    #[test]
    fn discovers_the_metadata_frontier_breadth_first_without_browse_demands() {
        let mut j = journal();
        assert_eq!(
            next_directory(&j, &[], &policy(), NOW).as_deref(),
            Some("root")
        );
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "b", "b", "root");
        let first = next_directory(&j, &[], &policy(), NOW + 1).unwrap();
        assert!(["a", "b"].contains(&first.as_str()));
        scanned(&mut j, &first, NOW + 1);
        directory(&mut j, "deep", &format!("{first}/deep"), &first);
        let second = next_directory(&j, &[], &policy(), NOW + 2).unwrap();
        assert_ne!(first, second);
        assert!(["a", "b"].contains(&second.as_str()));
        scanned(&mut j, &second, NOW + 2);
        assert_eq!(
            next_directory(&j, &[], &policy(), NOW + 3).as_deref(),
            Some("deep")
        );
        scanned(&mut j, "deep", NOW + 3);
        assert!(next_directory(&j, &[], &policy(), NOW + 4).is_none());
    }

    #[test]
    fn depth_limit_is_root_zero_and_can_be_crossed_by_an_explicit_request() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "deep", "a/deep", "a");
        let mut policy = policy();
        policy.prefetch_depth = Some(0);
        assert!(next_directory(&j, &[], &policy, NOW + 1).is_none());
        policy.prefetch_depth = Some(1);
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 1).as_deref(),
            Some("a")
        );
        scanned(&mut j, "a", NOW + 1);
        assert!(next_directory(&j, &[], &policy, NOW + 2).is_none());
        assert_eq!(
            next_directory(&j, &[demand("deep", "a/deep", NOW + 2)], &policy, NOW + 2).as_deref(),
            Some("deep")
        );
    }

    #[test]
    fn disabled_preindex_still_initializes_root_and_follows_inherited_pins() {
        let mut j = journal();
        let mut policy = policy();
        policy.enabled = false;
        policy.prefetch_depth = Some(0);
        assert_eq!(
            next_directory(&j, &[], &policy, NOW).as_deref(),
            Some("root")
        );
        scanned(&mut j, "root", NOW);
        directory(&mut j, "pinned", "pinned", "root");
        directory(&mut j, "child", "pinned/child", "pinned");
        directory(&mut j, "other", "other", "root");
        assert!(next_directory(&j, &[], &policy, NOW + 1).is_none());
        j.pins.insert("pinned".into());
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 1).as_deref(),
            Some("pinned")
        );
        scanned(&mut j, "pinned", NOW + 1);
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 2).as_deref(),
            Some("child")
        );
        scanned(&mut j, "child", NOW + 2);
        assert!(next_directory(&j, &[], &policy, NOW + 3).is_none());
        j.pins.insert("root".into());
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 3).as_deref(),
            Some("other")
        );
    }

    #[test]
    fn an_individually_pinned_file_keeps_its_parent_fresh_without_crawling_siblings() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "folder", "folder", "root");
        directory(&mut j, "file", "folder/file", "folder");
        j.items.get_mut("file").unwrap().item.kind = "file".into();
        directory(&mut j, "other", "folder/other", "folder");
        j.pins.insert("file".into());
        let mut policy = policy();
        policy.enabled = false;
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 1).as_deref(),
            Some("folder")
        );
        scanned(&mut j, "folder", NOW + 1);
        assert!(next_directory(&j, &[], &policy, NOW + 2).is_none());
        assert_eq!(
            next_directory(&j, &[], &policy, NOW + 30_001).as_deref(),
            Some("folder")
        );
    }

    #[test]
    fn requested_unseen_scope_precedes_cold_root_and_shallower_precrawl() {
        let mut j = journal();
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "deep", "a/deep", "a");
        assert_eq!(
            next_directory(&j, &[demand("deep", "a/deep", NOW)], &policy(), NOW).as_deref(),
            Some("deep")
        );
        assert_eq!(
            next_directory(&j, &[], &policy(), NOW).as_deref(),
            Some("root")
        );
    }

    #[test]
    fn success_times_enforce_active_and_general_refresh_intervals() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        scanned(&mut j, "a", NOW);
        let requests = [demand("a", "a", NOW + 1)];
        assert!(next_directory(&j, &requests, &policy(), NOW + 29_999).is_none());
        assert_eq!(
            next_directory(&j, &requests, &policy(), NOW + 30_000).as_deref(),
            Some("a")
        );
        assert!(next_directory(&j, &[], &policy(), NOW + 299_999).is_none());
        assert!(next_directory(&j, &[], &policy(), NOW + 300_000).is_some());
    }

    #[test]
    fn cached_stale_root_is_revalidated_before_new_background_descendants() {
        let mut j = journal();
        scanned(&mut j, "root", NOW - 300_000);
        directory(&mut j, "a", "a", "root");
        j.directory_turn = 0;
        assert_eq!(
            next_directory(&j, &[], &policy(), NOW).as_deref(),
            Some("root")
        );
        scanned(&mut j, "root", NOW);
        assert_eq!(
            next_directory(&j, &[], &policy(), NOW + 1).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn retry_backoff_cannot_be_bypassed_by_forced_requests_or_pins() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "failed", "failed", "root");
        directory(&mut j, "ready", "ready", "root");
        j.pins.insert("failed".into());
        j.directory_attempts.insert(
            "failed".into(),
            ScanAttempt {
                last: NOW,
                failures: 4,
                next_try: NOW + 60_000,
            },
        );
        let mut request = demand("failed", "failed", NOW + 1);
        request.force = true;
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy(), NOW + 1).as_deref(),
            Some("ready")
        );
        scanned(&mut j, "ready", NOW + 1);
        assert!(next_directory(&j, &[request.clone()], &policy(), NOW + 59_999).is_none());
        assert_eq!(
            next_directory(&j, &[request], &policy(), NOW + 60_000).as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn hot_forced_refresh_cannot_starve_precrawl_or_an_old_quiet_directory() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "hot", "hot", "root");
        directory(&mut j, "quiet", "quiet", "root");
        directory(&mut j, "new", "new", "root");
        scanned(&mut j, "hot", NOW);
        scanned(&mut j, "quiet", NOW - 500_000);
        j.directory_turn = 0;
        let mut selected = Vec::new();
        for turn in 1..=8 {
            let now = NOW + turn * 3_001;
            let mut request = demand("hot", "hot", now);
            request.force = true;
            let id = next_directory(&j, &[request], &policy(), now).unwrap();
            selected.push(id.clone());
            scanned(&mut j, &id, now);
        }
        assert!(selected[..4].iter().any(|id| id == "quiet"), "{selected:?}");
        assert!(selected[..4].iter().any(|id| id == "new"), "{selected:?}");
        assert!(selected.iter().any(|id| id == "hot"));
    }

    #[test]
    fn recursive_manual_refresh_crosses_policy_bounds_once_and_uses_path_boundaries() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "child", "a/child", "a");
        directory(&mut j, "sibling", "ab", "root");
        scanned(&mut j, "a", NOW);
        scanned(&mut j, "child", NOW);
        let mut policy = policy();
        policy.enabled = false;
        policy.prefetch_depth = Some(0);
        let mut request = demand("a", "a", NOW + 4_000);
        request.force = true;
        request.recursive = true;
        let mut selected = Vec::new();
        for offset in 0..2 {
            let now = NOW + 4_000 + offset;
            let id = next_directory(&j, &[request.clone()], &policy, now).unwrap();
            assert!(["a", "child"].contains(&id.as_str()));
            selected.push(id.clone());
            scanned(&mut j, &id, now + 1);
        }
        assert_ne!(selected[0], selected[1]);
        directory(&mut j, "new_child", "a/child/new", "child");
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy, NOW + 4_002).as_deref(),
            Some("new_child")
        );
        scanned(&mut j, "new_child", NOW + 4_003);
        assert!(next_directory(&j, &[request], &policy, NOW + 40_000).is_none());
    }

    #[test]
    fn completed_recursive_request_cannot_reactivate_for_a_future_unseen_descendant() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "child", "a/child", "a");
        let mut request = demand("a", "a", NOW + 4_000);
        request.force = true;
        request.recursive = true;
        scanned(&mut j, "a", NOW + 4_001);
        scanned(&mut j, "child", NOW + 4_002);
        let markers = completed_manual_demands(&j, &[request.clone()], false);
        assert_eq!(markers, vec![("a".into(), NOW + 4_000)]);
        j.completed_demands.extend(markers);
        directory(&mut j, "future", "a/child/future", "child");
        let mut policy = policy();
        policy.enabled = false;
        policy.prefetch_depth = Some(0);
        assert!(next_directory(&j, &[request.clone()], &policy, NOW + 40_000).is_none());
        request.requested = NOW + 40_001;
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy, NOW + 40_001).as_deref(),
            Some("future")
        );
        assert!(completed_manual_demands(&j, &[request], false).is_empty());
    }

    #[test]
    fn manual_completion_requires_each_known_directory_to_succeed_after_the_request() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "child", "a/child", "a");
        directory(&mut j, "sibling", "ab", "root");
        let mut request = demand("a", "a", NOW + 4_000);
        request.force = true;
        request.recursive = true;
        scanned(&mut j, "a", NOW + 4_001);
        assert!(completed_manual_demands(&j, &[request.clone()], false).is_empty());
        scanned(&mut j, "child", NOW);
        j.directory_attempts.get_mut("child").unwrap().failures = 1;
        assert!(completed_manual_demands(&j, &[request.clone()], false).is_empty());
        scanned(&mut j, "child", NOW + 4_002);
        j.directory_attempts.get_mut("child").unwrap().failures = 1;
        assert!(completed_manual_demands(&j, &[request.clone()], false).is_empty());
        scanned(&mut j, "child", NOW + 4_003);
        assert_eq!(
            completed_manual_demands(&j, &[request.clone()], false),
            vec![("a".into(), NOW + 4_000)]
        );
        // Non-recursive refresh consumes its own scope without scanning children.
        j.directory_scans.remove("child");
        request.recursive = false;
        assert_eq!(
            completed_manual_demands(&j, &[request], false),
            vec![("a".into(), NOW + 4_000)]
        );
    }

    #[test]
    fn managed_snapshot_completes_recursive_root_work_without_per_directory_scans() {
        let mut j = journal();
        directory(&mut j, "a", "a", "root");
        directory(&mut j, "child", "a/child", "a");
        let mut request = demand("root", "", NOW + 4_000);
        request.force = true;
        request.recursive = true;
        assert!(completed_manual_demands(&j, &[request.clone()], true).is_empty());
        scanned(&mut j, "root", NOW + 4_001);
        assert!(completed_manual_demands(&j, &[request.clone()], false).is_empty());
        assert_eq!(
            completed_manual_demands(&j, &[request], true),
            vec![("root".into(), NOW + 4_000)]
        );
    }

    #[test]
    fn unfinished_manual_refresh_retries_a_later_failed_scope_after_its_backoff() {
        let mut j = journal();
        directory(&mut j, "child", "child", "root");
        let mut request = demand("root", "", NOW + 4_000);
        request.force = true;
        request.recursive = true;
        scanned(&mut j, "root", NOW + 4_001);
        j.directory_attempts.insert(
            "root".into(),
            ScanAttempt {
                last: NOW + 5_000,
                failures: 1,
                next_try: NOW + 10_000,
            },
        );
        let mut policy = policy();
        policy.enabled = false;
        policy.prefetch_depth = Some(0);
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy, NOW + 9_000).as_deref(),
            Some("child")
        );
        scanned(&mut j, "child", NOW + 9_001);
        assert!(completed_manual_demands(&j, &[request.clone()], false).is_empty());
        assert!(next_directory(&j, &[request.clone()], &policy, NOW + 9_999).is_none());
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy, NOW + 10_000).as_deref(),
            Some("root")
        );
        scanned(&mut j, "root", NOW + 10_001);
        assert_eq!(
            completed_manual_demands(&j, &[request], false),
            vec![("root".into(), NOW + 4_000)]
        );
    }

    #[test]
    fn force_bypasses_ttl_once_and_exact_ties_rotate_with_the_durable_turn() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        scanned(&mut j, "a", NOW);
        let mut request = demand("a", "a", NOW + 3_001);
        request.force = true;
        assert_eq!(
            next_directory(&j, &[request.clone()], &policy(), NOW + 3_001).as_deref(),
            Some("a")
        );
        scanned(&mut j, "a", NOW + 3_002);
        assert!(next_directory(&j, &[request], &policy(), NOW + 40_000).is_none());
        directory(&mut j, "b", "b", "root");
        directory(&mut j, "c", "c", "root");
        j.directory_turn = 0;
        let first = next_directory(&j, &[], &policy(), NOW + 40_000).unwrap();
        j.directory_turn = 1;
        assert_ne!(
            next_directory(&j, &[], &policy(), NOW + 40_000).unwrap(),
            first
        );
    }

    #[test]
    fn separate_browse_hint_stays_active_after_one_shot_manual_refresh_completes() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        scanned(&mut j, "a", NOW);
        let mut manual = demand("a", "a", NOW + 4_000);
        manual.force = true;
        manual.recursive = true;
        let browse = demand("a", "a", NOW + 4_001);
        let hints = [manual, browse];
        let mut policy = policy();
        policy.enabled = false;
        assert_eq!(
            next_directory(&j, &hints, &policy, NOW + 4_001).as_deref(),
            Some("a")
        );
        scanned(&mut j, "a", NOW + 4_002);
        j.completed_demands
            .extend(completed_manual_demands(&j, &hints, false));
        assert!(next_directory(&j, &hints, &policy, NOW + 34_001).is_none());
        assert_eq!(
            next_directory(&j, &hints, &policy, NOW + 34_002).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn metadata_working_set_stale_path_and_other_binding_do_not_expand_the_tree() {
        let mut j = journal();
        scanned(&mut j, "root", NOW);
        directory(&mut j, "a", "a", "root");
        let mut requests = vec![demand("working_set", "", NOW), demand("a", "old-a", NOW)];
        let mut metadata = demand("a", "a", NOW);
        metadata.metadata = true;
        requests.push(metadata);
        let mut wrong_binding = demand("a", "a", NOW);
        wrong_binding.binding = "other".into();
        requests.push(wrong_binding);
        let mut policy = policy();
        policy.enabled = false;
        assert!(next_directory(&j, &requests, &policy, NOW + 1).is_none());
    }
}
