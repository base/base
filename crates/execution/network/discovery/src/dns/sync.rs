use std::time::{Duration, Instant};

use enr::EnrKeyUnambiguous;
use linked_hash_set::LinkedHashSet;
use secp256k1::SecretKey;

use crate::dns::tree::{DnsLinkEntry, DnsTreeRootEntry};

/// A sync-able tree
#[derive(Debug)]
pub struct DnsSyncTree<K: EnrKeyUnambiguous = SecretKey> {
    /// Root of the tree
    root: DnsTreeRootEntry,
    /// Link to this tree
    link: DnsLinkEntry<K>,
    /// Timestamp when the root was updated
    root_updated: Instant,
    /// The state of the tree sync progress.
    sync_state: SyncState,
    /// Unresolved links of the tree
    unresolved_links: LinkedHashSet<String>,
    /// Unresolved nodes of the tree
    unresolved_nodes: LinkedHashSet<String>,
}

// === impl DnsSyncTree ===

impl<K: EnrKeyUnambiguous> DnsSyncTree<K> {
    pub fn new(root: DnsTreeRootEntry, link: DnsLinkEntry<K>) -> Self {
        Self {
            root,
            link,
            root_updated: Instant::now(),
            sync_state: SyncState::Pending,
            unresolved_links: Default::default(),
            unresolved_nodes: Default::default(),
        }
    }

    #[cfg(test)]
    pub const fn root(&self) -> &DnsTreeRootEntry {
        &self.root
    }

    pub const fn link(&self) -> &DnsLinkEntry<K> {
        &self.link
    }

    pub fn extend_children(
        &mut self,
        kind: DnsResolveKind,
        children: impl IntoIterator<Item = String>,
    ) {
        match kind {
            DnsResolveKind::Enr => {
                self.unresolved_nodes.extend(children);
            }
            DnsResolveKind::Link => {
                self.unresolved_links.extend(children);
            }
        }
    }

    /// Advances the state of the tree by returning actions to perform
    pub fn poll(&mut self, now: Instant, update_timeout: Duration) -> Option<DnsSyncAction> {
        match self.sync_state {
            SyncState::Pending => {
                self.sync_state = SyncState::Enr;
                return Some(DnsSyncAction::Link(self.root.link_root.clone()));
            }
            SyncState::Enr => {
                self.sync_state = SyncState::Active;
                return Some(DnsSyncAction::Enr(self.root.enr_root.clone()));
            }
            SyncState::Link => {
                self.sync_state = SyncState::Active;
                return Some(DnsSyncAction::Link(self.root.link_root.clone()));
            }
            SyncState::Active => {
                if now > self.root_updated + update_timeout {
                    self.sync_state = SyncState::RootUpdate;
                    return Some(DnsSyncAction::UpdateRoot);
                }
            }
            SyncState::RootUpdate => return None,
        }

        if let Some(link) = self.unresolved_links.pop_front() {
            return Some(DnsSyncAction::Link(link));
        }

        let enr = self.unresolved_nodes.pop_front()?;
        Some(DnsSyncAction::Enr(enr))
    }

    /// Updates the root and returns what changed
    pub fn update_root(&mut self, root: DnsTreeRootEntry) {
        let enr_unchanged = root.enr_root == self.root.enr_root;
        let link_unchanged = root.link_root == self.root.link_root;

        self.root = root;
        self.root_updated = Instant::now();

        let state = match (enr_unchanged, link_unchanged) {
            // both unchanged — no resync needed
            (true, true) => return,
            // only ENR changed
            (false, true) => {
                self.unresolved_nodes.clear();
                SyncState::Enr
            }
            // only LINK changed
            (true, false) => {
                self.unresolved_links.clear();
                SyncState::Link
            }
            // both changed
            (false, false) => {
                self.unresolved_nodes.clear();
                self.unresolved_links.clear();
                SyncState::Pending
            }
        };
        self.sync_state = state;
    }
}

/// The action to perform by the service
#[derive(Debug)]
pub enum DnsSyncAction {
    UpdateRoot,
    Enr(String),
    Link(String),
}

/// How the [`DnsSyncTree::update_root`] changed the root
#[derive(Debug)]
enum SyncState {
    RootUpdate,
    Pending,
    Enr,
    Link,
    Active,
}

/// What kind of hash to resolve
#[derive(Debug)]
pub enum DnsResolveKind {
    Enr,
    Link,
}

// === impl DnsResolveKind ===

impl DnsResolveKind {
    pub const fn is_link(&self) -> bool {
        matches!(self, Self::Link)
    }
}

#[cfg(test)]
mod tests {
    use enr::EnrKey;
    use secp256k1::rand::thread_rng;

    use super::*;

    fn base_root() -> DnsTreeRootEntry {
        // taken from existing tests to ensure valid formatting
        let s = "enrtree-root:v1 e=QFT4PBCRX4XQCV3VUYJ6BTCEPU l=JGUFMSAGI7KZYB3P7IZW4S5Y3A seq=3 sig=3FmXuVwpa8Y7OstZTx9PIb1mt8FrW7VpDOFv4AaGCsZ2EIHmhraWhe4NxYhQDlw5MjeFXYMbJjsPeKlHzmJREQE";
        s.parse::<DnsTreeRootEntry>().unwrap()
    }

    fn make_tree() -> DnsSyncTree {
        let secret_key = SecretKey::new(&mut thread_rng());
        let link =
            DnsLinkEntry { domain: "nodes.example.org".to_string(), pubkey: secret_key.public() };
        DnsSyncTree::new(base_root(), link)
    }

    fn advance_to_active(tree: &mut DnsSyncTree) {
        // Move Pending -> (emit Link) -> Enr, then Enr -> (emit Enr) -> Active
        let now = Instant::now();
        let timeout = Duration::from_secs(60 * 60 * 24);
        let _ = tree.poll(now, timeout);
        let _ = tree.poll(now, timeout);
    }

    #[test]
    fn update_root_unchanged_no_action_from_active() {
        let mut tree = make_tree();
        let now = Instant::now();
        let timeout = Duration::from_secs(60 * 60 * 24);
        advance_to_active(&mut tree);

        // same root -> no resync
        let same = base_root();
        tree.update_root(same);
        assert!(tree.poll(now, timeout).is_none());
    }

    #[test]
    fn update_root_only_enr_changed_triggers_enr() {
        let mut tree = make_tree();
        advance_to_active(&mut tree);
        let mut new_root = base_root();
        new_root.enr_root = "NEW_ENR_ROOT".to_string();
        let now = Instant::now();
        let timeout = Duration::from_secs(60 * 60 * 24);

        tree.update_root(new_root.clone());
        match tree.poll(now, timeout) {
            Some(DnsSyncAction::Enr(hash)) => assert_eq!(hash, new_root.enr_root),
            other => panic!("expected Enr action, got {:?}", other),
        }
    }

    #[test]
    fn update_root_only_link_changed_triggers_link() {
        let mut tree = make_tree();
        advance_to_active(&mut tree);
        let mut new_root = base_root();
        new_root.link_root = "NEW_LINK_ROOT".to_string();
        let now = Instant::now();
        let timeout = Duration::from_secs(60 * 60 * 24);

        tree.update_root(new_root.clone());
        match tree.poll(now, timeout) {
            Some(DnsSyncAction::Link(hash)) => assert_eq!(hash, new_root.link_root),
            other => panic!("expected Link action, got {:?}", other),
        }
    }

    #[test]
    fn update_root_both_changed_triggers_link_then_enr() {
        let mut tree = make_tree();
        advance_to_active(&mut tree);
        let mut new_root = base_root();
        new_root.enr_root = "NEW_ENR_ROOT".to_string();
        new_root.link_root = "NEW_LINK_ROOT".to_string();
        let now = Instant::now();
        let timeout = Duration::from_secs(60 * 60 * 24);

        tree.update_root(new_root.clone());
        match tree.poll(now, timeout) {
            Some(DnsSyncAction::Link(hash)) => assert_eq!(hash, new_root.link_root),
            other => panic!("expected first Link action, got {:?}", other),
        }
        match tree.poll(now, timeout) {
            Some(DnsSyncAction::Enr(hash)) => assert_eq!(hash, new_root.enr_root),
            other => panic!("expected second Enr action, got {:?}", other),
        }
    }
}
