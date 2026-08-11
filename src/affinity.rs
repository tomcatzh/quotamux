use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};

const HASH_DOMAIN: &[u8] = b"quotamux-prefix-affinity-v1\0";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrefixAffinityConfig {
    pub checkpoint_bytes: usize,
    pub max_checkpoints_per_path: usize,
    pub max_candidates_per_prefix: usize,
    pub max_leases: usize,
    pub success_ttl_ms: i64,
}

impl Default for PrefixAffinityConfig {
    fn default() -> Self {
        Self {
            checkpoint_bytes: 128,
            max_checkpoints_per_path: 4_096,
            max_candidates_per_prefix: 8,
            max_leases: 16_384,
            success_ttl_ms: 5 * 60 * 1000,
        }
    }
}

impl PrefixAffinityConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.checkpoint_bytes == 0 {
            return Err("affinity.checkpoint_bytes must be greater than zero".into());
        }
        if self.max_checkpoints_per_path == 0 {
            return Err("affinity.max_checkpoints_per_path must be greater than zero".into());
        }
        if self.max_candidates_per_prefix == 0 {
            return Err("affinity.max_candidates_per_prefix must be greater than zero".into());
        }
        if self.max_leases == 0 {
            return Err("affinity.max_leases must be greater than zero".into());
        }
        if self.success_ttl_ms <= 0 {
            return Err("affinity.success_ttl_ms must be greater than zero".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PrefixKey {
    pub namespace: [u8; 16],
    pub ordinal: u32,
    pub length: u64,
    pub fingerprint: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FingerprintPath {
    pub namespace: [u8; 16],
    pub checkpoints: Vec<PrefixKey>,
    pub total_bytes: u64,
    pub final_fingerprint: [u8; 16],
}

impl FingerprintPath {
    pub fn deepest_ordinal(&self) -> Option<u32> {
        self.checkpoints.last().map(|checkpoint| checkpoint.ordinal)
    }
}

#[derive(Clone)]
pub struct PrefixFingerprinter {
    key: [u8; 32],
    checkpoint_bytes: usize,
    max_checkpoints: usize,
}

impl PrefixFingerprinter {
    pub fn new(key: [u8; 32], checkpoint_bytes: usize) -> Result<Self, String> {
        if checkpoint_bytes == 0 {
            return Err("checkpoint_bytes must be greater than zero".into());
        }
        Ok(Self {
            key,
            checkpoint_bytes,
            max_checkpoints: usize::MAX,
        })
    }

    pub fn new_bounded(
        key: [u8; 32],
        checkpoint_bytes: usize,
        max_checkpoints: usize,
    ) -> Result<Self, String> {
        if max_checkpoints == 0 {
            return Err("max_checkpoints must be greater than zero".into());
        }
        let mut fingerprinter = Self::new(key, checkpoint_bytes)?;
        fingerprinter.max_checkpoints = max_checkpoints;
        Ok(fingerprinter)
    }

    pub fn fingerprint<'a, I>(&self, namespace: &[u8], chunks: I) -> FingerprintPath
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let namespace_hash = keyed_128(&self.key, b"namespace\0", namespace);
        let mut hasher = blake3::Hasher::new_keyed(&self.key);
        hasher.update(HASH_DOMAIN);
        hasher.update(&(namespace.len() as u64).to_le_bytes());
        hasher.update(namespace);

        let mut checkpoints = Vec::new();
        let mut total = 0_u64;
        let interval = self.checkpoint_bytes as u64;
        for chunk in chunks {
            let mut remaining = chunk;
            while !remaining.is_empty() {
                let until_boundary = (interval - total % interval) as usize;
                let take = remaining.len().min(until_boundary);
                hasher.update(&remaining[..take]);
                total += take as u64;
                remaining = &remaining[take..];
                if total.is_multiple_of(interval) && checkpoints.len() < self.max_checkpoints {
                    checkpoints.push(checkpoint(
                        namespace_hash,
                        checkpoints.len() as u32 + 1,
                        total,
                        &hasher,
                    ));
                }
            }
        }
        if total > 0 && !total.is_multiple_of(interval) && checkpoints.len() < self.max_checkpoints
        {
            checkpoints.push(checkpoint(
                namespace_hash,
                checkpoints.len() as u32 + 1,
                total,
                &hasher,
            ));
        }
        let mut final_fingerprint = [0_u8; 16];
        final_fingerprint.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        FingerprintPath {
            namespace: namespace_hash,
            checkpoints,
            total_bytes: total,
            final_fingerprint,
        }
    }
}

fn checkpoint(
    namespace: [u8; 16],
    ordinal: u32,
    length: u64,
    hasher: &blake3::Hasher,
) -> PrefixKey {
    let mut fingerprint = [0_u8; 16];
    fingerprint.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    PrefixKey {
        namespace,
        ordinal,
        length,
        fingerprint,
    }
}

fn keyed_128(key: &[u8; 32], tag: &[u8], value: &[u8]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(HASH_DOMAIN);
    hasher.update(tag);
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
    let mut result = [0_u8; 16];
    result.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    result
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CacheDomain {
    pub id: String,
    pub generation: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceConfidence {
    Pending,
    SuccessfulRequest,
    ProviderReported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WarmEpoch {
    pub through_checkpoint: u32,
    pub expires_at_ms: i64,
    pub cached_tokens: u64,
    pub confidence: EvidenceConfidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheEvidence {
    pub through_checkpoint: u32,
    pub expires_at_ms: i64,
    pub cached_tokens: u64,
    pub confidence: EvidenceConfidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Lease {
    pub id: String,
    pub domain: CacheDomain,
    pub path: FingerprintPath,
    pub epochs: Vec<WarmEpoch>,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AffinityMatch {
    pub lease_id: String,
    pub domain: CacheDomain,
    pub matched_checkpoint: u32,
    pub matched_bytes: u64,
    pub expires_at_ms: i64,
    pub cached_tokens: u64,
    pub confidence: EvidenceConfidence,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExpireResult {
    pub removed_epochs: usize,
    pub removed_leases: usize,
    pub updated_leases: Vec<Lease>,
    pub deleted_lease_ids: Vec<String>,
}

#[derive(Default)]
struct IndexState {
    leases: HashMap<String, Lease>,
    prefix: HashMap<PrefixKey, Vec<String>>,
    expiry_order: BTreeSet<(i64, String)>,
}

pub struct PrefixIndex {
    config: PrefixAffinityConfig,
    lease_key: [u8; 32],
    state: RwLock<IndexState>,
}

impl PrefixIndex {
    pub fn new(config: PrefixAffinityConfig, lease_key: [u8; 32]) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            lease_key,
            state: RwLock::new(IndexState::default()),
        })
    }

    pub fn observe(
        &self,
        path: FingerprintPath,
        domain: CacheDomain,
        evidence: CacheEvidence,
        now_ms: i64,
    ) -> Result<Option<Lease>, String> {
        if path.checkpoints.is_empty() {
            return Ok(None);
        }
        if evidence.through_checkpoint == 0
            || evidence.through_checkpoint > path.deepest_ordinal().unwrap_or(0)
        {
            return Err("cache evidence frontier is outside the fingerprint path".into());
        }
        if evidence.expires_at_ms <= now_ms {
            return Ok(None);
        }
        let id = lease_id(&self.lease_key, &domain, &path);
        let epoch = WarmEpoch {
            through_checkpoint: evidence.through_checkpoint,
            expires_at_ms: evidence.expires_at_ms,
            cached_tokens: evidence.cached_tokens,
            confidence: evidence.confidence,
        };
        let mut state = self.state.write().expect("affinity index poisoned");
        let previous_expiry = state.leases.get(&id).and_then(lease_expiry);
        let updated = {
            let lease = state.leases.entry(id.clone()).or_insert_with(|| Lease {
                id: id.clone(),
                domain,
                path,
                epochs: Vec::new(),
                version: 0,
            });
            if lease
                .epochs
                .iter()
                .any(|existing| dominates(existing, &epoch))
            {
                return Ok(None);
            }
            lease.epochs.retain(|existing| !dominates(&epoch, existing));
            lease.epochs.push(epoch);
            compact_epochs(&mut lease.epochs);
            lease.version = lease.version.saturating_add(1);
            lease.clone()
        };
        if let Some(expires_at) = previous_expiry {
            state.expiry_order.remove(&(expires_at, id.clone()));
        }
        state.expiry_order.insert((
            lease_expiry(&updated).expect("observed lease has an epoch"),
            id.clone(),
        ));
        for key in updated
            .path
            .checkpoints
            .iter()
            .take_while(|key| key.ordinal <= evidence.through_checkpoint)
        {
            add_prefix_ref(
                &mut state,
                key,
                &id,
                self.config.max_candidates_per_prefix,
                now_ms,
            );
        }
        while state.leases.len() > self.config.max_leases {
            let Some((_, oldest_id)) = state.expiry_order.first().cloned() else {
                break;
            };
            remove_lease(&mut state, &oldest_id);
        }
        Ok(state.leases.contains_key(&id).then_some(updated))
    }

    pub fn lookup(
        &self,
        path: &FingerprintPath,
        eligible: &HashSet<CacheDomain>,
        now_ms: i64,
    ) -> Option<AffinityMatch> {
        let state = self.state.read().expect("affinity index poisoned");
        for key in path.checkpoints.iter().rev() {
            let Some(ids) = state.prefix.get(key) else {
                continue;
            };
            let best = ids
                .iter()
                .filter_map(|id| state.leases.get(id))
                .filter(|lease| eligible.contains(&lease.domain))
                .filter_map(|lease| {
                    best_epoch(&lease.epochs, key.ordinal, now_ms).map(|epoch| AffinityMatch {
                        lease_id: lease.id.clone(),
                        domain: lease.domain.clone(),
                        matched_checkpoint: key.ordinal,
                        matched_bytes: key.length,
                        expires_at_ms: epoch.expires_at_ms,
                        cached_tokens: epoch.cached_tokens,
                        confidence: epoch.confidence,
                    })
                })
                .max_by(|left, right| match_score(left).cmp(&match_score(right)));
            if best.is_some() {
                return best;
            }
        }
        None
    }

    pub fn expire(&self, now_ms: i64) -> ExpireResult {
        let mut state = self.state.write().expect("affinity index poisoned");
        let mut result = ExpireResult::default();
        let ids = state.leases.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let Some(previous_expiry) = state.leases.get(&id).and_then(lease_expiry) else {
                continue;
            };
            let (before, updated) = {
                let lease = state.leases.get_mut(&id).expect("lease exists");
                let before = lease.epochs.len();
                lease.epochs.retain(|epoch| epoch.expires_at_ms > now_ms);
                if lease.epochs.is_empty() {
                    (before, None)
                } else {
                    if before != lease.epochs.len() {
                        compact_epochs(&mut lease.epochs);
                        lease.version = lease.version.saturating_add(1);
                    }
                    (before, Some(lease.clone()))
                }
            };
            let after = updated.as_ref().map_or(0, |lease| lease.epochs.len());
            result.removed_epochs += before - after;
            let Some(updated) = updated else {
                remove_lease(&mut state, &id);
                result.removed_leases += 1;
                result.deleted_lease_ids.push(id);
                continue;
            };
            if before != after {
                state.expiry_order.remove(&(previous_expiry, id.clone()));
                state.expiry_order.insert((
                    lease_expiry(&updated).expect("retained lease has an epoch"),
                    id.clone(),
                ));
                let max_frontier = updated
                    .epochs
                    .iter()
                    .map(|epoch| epoch.through_checkpoint)
                    .max()
                    .unwrap_or(0);
                for key in updated
                    .path
                    .checkpoints
                    .iter()
                    .filter(|key| key.ordinal > max_frontier)
                {
                    remove_prefix_ref(&mut state.prefix, key, &id);
                }
                result.updated_leases.push(updated);
            }
        }
        result
    }

    pub fn leases(&self) -> Vec<Lease> {
        let state = self.state.read().expect("affinity index poisoned");
        let mut leases = state.leases.values().cloned().collect::<Vec<_>>();
        leases.sort_by(|left, right| left.id.cmp(&right.id));
        leases
    }

    pub fn prefix_candidate_count(&self, key: &PrefixKey) -> usize {
        self.state
            .read()
            .expect("affinity index poisoned")
            .prefix
            .get(key)
            .map_or(0, Vec::len)
    }
}

fn lease_id(key: &[u8; 32], domain: &CacheDomain, path: &FingerprintPath) -> String {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(HASH_DOMAIN);
    for value in [&domain.id, &domain.generation] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(&path.namespace);
    hasher.update(&path.total_bytes.to_le_bytes());
    hasher.update(&path.final_fingerprint);
    hasher.finalize().to_hex()[..32].to_string()
}

fn lease_expiry(lease: &Lease) -> Option<i64> {
    lease.epochs.iter().map(|epoch| epoch.expires_at_ms).max()
}

fn dominates(left: &WarmEpoch, right: &WarmEpoch) -> bool {
    left.through_checkpoint >= right.through_checkpoint && left.expires_at_ms >= right.expires_at_ms
}

fn compact_epochs(epochs: &mut Vec<WarmEpoch>) {
    let existing = epochs.clone();
    epochs.retain(|candidate| {
        !existing
            .iter()
            .any(|other| other != candidate && dominates(other, candidate))
    });
    epochs.sort_by_key(|epoch| (epoch.through_checkpoint, epoch.expires_at_ms));
    epochs.dedup();
}

fn best_epoch(epochs: &[WarmEpoch], ordinal: u32, now_ms: i64) -> Option<&WarmEpoch> {
    epochs
        .iter()
        .filter(|epoch| epoch.through_checkpoint >= ordinal && epoch.expires_at_ms > now_ms)
        .max_by_key(|epoch| {
            (
                epoch.confidence,
                epoch.expires_at_ms,
                epoch.through_checkpoint,
            )
        })
}

fn match_score(value: &AffinityMatch) -> (EvidenceConfidence, i64, &str) {
    (
        value.confidence,
        value.expires_at_ms,
        value.lease_id.as_str(),
    )
}

fn add_prefix_ref(
    state: &mut IndexState,
    key: &PrefixKey,
    id: &str,
    max_candidates: usize,
    now_ms: i64,
) {
    let ids = state.prefix.entry(*key).or_default();
    if !ids.iter().any(|existing| existing == id) {
        ids.push(id.to_string());
    }
    if ids.len() <= max_candidates {
        return;
    }
    let remove = ids
        .iter()
        .enumerate()
        .min_by_key(|(_, candidate)| {
            state
                .leases
                .get(*candidate)
                .and_then(|lease| best_epoch(&lease.epochs, key.ordinal, now_ms))
                .map(|epoch| {
                    (
                        epoch.confidence,
                        epoch.expires_at_ms,
                        epoch.through_checkpoint,
                        candidate.as_str(),
                    )
                })
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    ids.remove(remove);
}

fn remove_prefix_ref(prefix: &mut HashMap<PrefixKey, Vec<String>>, key: &PrefixKey, id: &str) {
    if let Some(ids) = prefix.get_mut(key) {
        ids.retain(|candidate| candidate != id);
        if ids.is_empty() {
            prefix.remove(key);
        }
    }
}

fn remove_all_refs(prefix: &mut HashMap<PrefixKey, Vec<String>>, lease: &Lease) {
    for key in &lease.path.checkpoints {
        remove_prefix_ref(prefix, key, &lease.id);
    }
}

fn remove_lease(state: &mut IndexState, id: &str) -> Option<Lease> {
    let removed = state.leases.remove(id)?;
    if let Some(expires_at) = lease_expiry(&removed) {
        state.expiry_order.remove(&(expires_at, id.to_string()));
    }
    remove_all_refs(&mut state.prefix, &removed);
    Some(removed)
}

pub struct AffinityDirectory {
    config: PrefixAffinityConfig,
    fingerprinter: PrefixFingerprinter,
    index: Arc<PrefixIndex>,
}

impl AffinityDirectory {
    pub fn new(config: PrefixAffinityConfig) -> Result<Self, String> {
        config.validate()?;
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key).map_err(|error| error.to_string())?;
        Self::with_key(config, key)
    }

    pub fn with_key(config: PrefixAffinityConfig, key: [u8; 32]) -> Result<Self, String> {
        config.validate()?;
        let fingerprinter = PrefixFingerprinter::new_bounded(
            key,
            config.checkpoint_bytes,
            config.max_checkpoints_per_path,
        )?;
        let index = Arc::new(PrefixIndex::new(config.clone(), key)?);
        Ok(Self {
            config,
            fingerprinter,
            index,
        })
    }

    pub fn fingerprint<'a, I>(&self, namespace: &[u8], chunks: I) -> FingerprintPath
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        self.fingerprinter.fingerprint(namespace, chunks)
    }

    pub fn lookup(
        &self,
        path: &FingerprintPath,
        eligible: &HashSet<CacheDomain>,
        now_ms: i64,
    ) -> Option<AffinityMatch> {
        self.index.lookup(path, eligible, now_ms)
    }

    pub fn observe(
        &self,
        path: FingerprintPath,
        domain: CacheDomain,
        evidence: CacheEvidence,
        now_ms: i64,
    ) -> Result<bool, String> {
        Ok(self
            .index
            .observe(path, domain, evidence, now_ms)?
            .is_some())
    }

    pub fn expire(&self, now_ms: i64) -> ExpireResult {
        self.index.expire(now_ms)
    }

    pub fn success_ttl_ms(&self) -> i64 {
        self.config.success_ttl_ms
    }

    pub fn lease_count(&self) -> usize {
        self.index.leases().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn config(interval: usize, candidates: usize) -> PrefixAffinityConfig {
        PrefixAffinityConfig {
            checkpoint_bytes: interval,
            max_candidates_per_prefix: candidates,
            ..PrefixAffinityConfig::default()
        }
    }

    fn domain(id: &str) -> CacheDomain {
        CacheDomain {
            id: id.into(),
            generation: "generation-1".into(),
        }
    }

    fn path(bytes: &[u8]) -> FingerprintPath {
        PrefixFingerprinter::new(KEY, 10)
            .unwrap()
            .fingerprint(b"model/chat-template-v1", [bytes])
    }

    fn evidence(frontier: u32, expires: i64) -> CacheEvidence {
        CacheEvidence {
            through_checkpoint: frontier,
            expires_at_ms: expires,
            cached_tokens: frontier as u64 * 10,
            confidence: EvidenceConfidence::ProviderReported,
        }
    }

    #[test]
    fn fingerprints_do_not_depend_on_input_chunk_partitioning() {
        let bytes = b"abcdefgh";
        let fingerprinter = PrefixFingerprinter::new(KEY, 3).unwrap();
        let expected = fingerprinter.fingerprint(b"namespace", [bytes.as_slice()]);
        for mask in 0..(1_u32 << (bytes.len() - 1)) {
            let mut chunks = Vec::new();
            let mut start = 0;
            for boundary in 1..bytes.len() {
                if mask & (1 << (boundary - 1)) != 0 {
                    chunks.push(&bytes[start..boundary]);
                    start = boundary;
                }
            }
            chunks.push(&bytes[start..]);
            let actual = fingerprinter.fingerprint(b"namespace", chunks);
            assert_eq!(actual, expected, "partition mask={mask:#09b}");
        }
    }

    #[test]
    fn namespace_whitespace_role_and_length_change_keys() {
        let fingerprinter = PrefixFingerprinter::new(KEY, 128).unwrap();
        let base = fingerprinter.fingerprint(b"model-a", [b"user\0hello".as_slice()]);
        let cases = [
            fingerprinter.fingerprint(b"model-b", [b"user\0hello".as_slice()]),
            fingerprinter.fingerprint(b"model-a", [b"user\0 hello".as_slice()]),
            fingerprinter.fingerprint(b"model-a", [b"tool\0hello".as_slice()]),
            fingerprinter.fingerprint(b"model-a", [b"user\0hello!".as_slice()]),
            fingerprinter.fingerprint(
                b"model-a",
                [b"user\0hello\0tool:{\"type\":\"string\"}".as_slice()],
            ),
        ];
        for changed in cases {
            assert_ne!(changed.checkpoints[0], base.checkpoints[0]);
        }
    }

    #[test]
    fn long_path_keeps_short_prefix_discoverable_for_a_branch() {
        let affinity_config = config(1_000, 8);
        let fingerprinter = PrefixFingerprinter::new(KEY, 1_000).unwrap();
        let index = PrefixIndex::new(affinity_config, KEY).unwrap();
        let mut long_bytes = vec![b'a'; 3_000];
        long_bytes.extend(vec![b'b'; 4_000]);
        let long = fingerprinter.fingerprint(b"model/chat-template-v1", [long_bytes.as_slice()]);
        index
            .observe(long.clone(), domain("worker-a"), evidence(7, 10_000), 0)
            .unwrap();
        let mut branch_bytes = vec![b'a'; 3_000];
        branch_bytes.extend(vec![b'c'; 3_000]);
        let branch =
            fingerprinter.fingerprint(b"model/chat-template-v1", [branch_bytes.as_slice()]);
        let eligible = HashSet::from([domain("worker-a")]);
        let found = index.lookup(&branch, &eligible, 1).unwrap();
        assert_eq!(found.domain.id, "worker-a");
        assert_eq!(found.matched_checkpoint, 3);
        assert_eq!(found.matched_bytes, 3_000);
    }

    #[test]
    fn short_new_epoch_does_not_extend_old_long_frontier() {
        let index = PrefixIndex::new(config(10, 8), KEY).unwrap();
        let request = path(b"01234567890123456789012345678901234567890123456789");
        index
            .observe(request.clone(), domain("worker"), evidence(5, 100), 0)
            .unwrap();
        index
            .observe(request.clone(), domain("worker"), evidence(2, 1_000), 10)
            .unwrap();
        let eligible = HashSet::from([domain("worker")]);
        let found = index.lookup(&request, &eligible, 200).unwrap();
        assert_eq!(found.matched_checkpoint, 2);
        assert_eq!(found.expires_at_ms, 1_000);
    }

    #[test]
    fn dominated_epochs_are_removed_but_crossed_epochs_remain() {
        let index = PrefixIndex::new(config(10, 8), KEY).unwrap();
        let request = path(b"01234567890123456789012345678901234567890123456789");
        index
            .observe(request.clone(), domain("worker"), evidence(2, 100), 0)
            .unwrap();
        index
            .observe(request.clone(), domain("worker"), evidence(4, 200), 0)
            .unwrap();
        index
            .observe(request, domain("worker"), evidence(5, 150), 0)
            .unwrap();
        let leases = index.leases();
        assert_eq!(leases[0].epochs.len(), 2);
        assert!(
            leases[0]
                .epochs
                .iter()
                .any(|epoch| epoch.through_checkpoint == 4)
        );
        assert!(
            leases[0]
                .epochs
                .iter()
                .any(|epoch| epoch.through_checkpoint == 5)
        );
    }

    #[test]
    fn logical_expiry_precedes_lazy_gc() {
        let index = PrefixIndex::new(config(10, 8), KEY).unwrap();
        let request = path(b"01234567890123456789");
        index
            .observe(request.clone(), domain("worker"), evidence(2, 100), 0)
            .unwrap();
        let eligible = HashSet::from([domain("worker")]);
        assert!(index.lookup(&request, &eligible, 100).is_none());
        assert_eq!(index.leases().len(), 1);
        let expired = index.expire(100);
        assert_eq!(expired.removed_epochs, 1);
        assert_eq!(expired.removed_leases, 1);
        assert!(index.leases().is_empty());
    }

    #[test]
    fn candidate_pruning_is_bounded_and_never_fabricates_coverage() {
        let index = PrefixIndex::new(config(10, 2), KEY).unwrap();
        let request = path(b"012345678901234567890123456789");
        for id in ["a", "b", "c"] {
            index
                .observe(request.clone(), domain(id), evidence(1, 1_000), 0)
                .unwrap();
        }
        assert_eq!(index.prefix_candidate_count(&request.checkpoints[0]), 2);
        let eligible = HashSet::from([domain("a"), domain("b"), domain("c")]);
        let found = index.lookup(&request, &eligible, 1).unwrap();
        assert_eq!(found.matched_checkpoint, 1);
        assert_eq!(found.matched_bytes, 10);
    }

    #[test]
    fn new_in_memory_directory_starts_empty() {
        let first = AffinityDirectory::with_key(config(10, 8), KEY).unwrap();
        let request = first.fingerprint(b"namespace", [b"01234567890123456789".as_slice()]);
        first
            .observe(request.clone(), domain("worker"), evidence(2, 1_000), 0)
            .unwrap();
        assert_eq!(first.lease_count(), 1);

        let restarted = AffinityDirectory::with_key(config(10, 8), KEY).unwrap();
        assert_eq!(restarted.lease_count(), 0);
        assert!(
            restarted
                .lookup(&request, &HashSet::from([domain("worker")]), 1)
                .is_none()
        );
    }

    #[test]
    fn path_and_directory_capacity_are_strictly_bounded() {
        let mut affinity_config = config(10, 8);
        affinity_config.max_checkpoints_per_path = 3;
        affinity_config.max_leases = 2;
        let directory = AffinityDirectory::with_key(affinity_config, KEY).unwrap();
        let paths = (*b"abc").map(|suffix| {
            let mut bytes = vec![b'x'; 100];
            bytes[99] = suffix;
            directory.fingerprint(b"namespace", [bytes.as_slice()])
        });
        assert!(paths.iter().all(|path| path.checkpoints.len() == 3));
        assert_ne!(paths[0].final_fingerprint, paths[1].final_fingerprint);

        for (index, request) in paths.iter().enumerate() {
            directory
                .observe(
                    request.clone(),
                    domain(&format!("worker-{index}")),
                    evidence(3, 1_000 + index as i64),
                    0,
                )
                .unwrap();
        }
        assert_eq!(directory.lease_count(), 2);
        assert!(
            directory
                .lookup(&paths[0], &HashSet::from([domain("worker-0")]), 1)
                .is_none()
        );
        for (index, request) in paths.iter().enumerate().skip(1) {
            assert!(
                directory
                    .lookup(
                        request,
                        &HashSet::from([domain(&format!("worker-{index}"))]),
                        1,
                    )
                    .is_some()
            );
        }
    }

    #[test]
    fn concurrent_observations_lookups_and_gc_remain_bounded() {
        let index = Arc::new(PrefixIndex::new(config(10, 4), KEY).unwrap());
        let request = path(b"012345678901234567890123456789");
        std::thread::scope(|scope| {
            for worker in 0..32 {
                let index = index.clone();
                let request = request.clone();
                scope.spawn(move || {
                    index
                        .observe(
                            request,
                            domain(&format!("worker-{worker}")),
                            evidence(3, 1_000 + worker),
                            0,
                        )
                        .unwrap();
                });
            }
        });
        let eligible = Arc::new(
            (0..32)
                .map(|worker| domain(&format!("worker-{worker}")))
                .collect::<HashSet<_>>(),
        );
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let index = index.clone();
                let request = request.clone();
                let eligible = eligible.clone();
                scope.spawn(move || {
                    for now in 900..1_050 {
                        let _ = index.lookup(&request, &eligible, now);
                    }
                });
            }
            let expiring = index.clone();
            scope.spawn(move || {
                let _ = expiring.expire(1_010);
            });
            for worker in 32..40 {
                let index = index.clone();
                let request = request.clone();
                scope.spawn(move || {
                    index
                        .observe(
                            request,
                            domain(&format!("worker-{worker}")),
                            evidence(3, 2_000 + worker),
                            1_000,
                        )
                        .unwrap();
                });
            }
        });
        assert!(
            request
                .checkpoints
                .iter()
                .all(|key| index.prefix_candidate_count(key) <= 4)
        );
    }
}
