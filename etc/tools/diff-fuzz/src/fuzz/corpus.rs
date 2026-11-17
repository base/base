//! The fuzz corpus: cases that added coverage, the features seen so far, and scheduling.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
};

use eyre::WrapErr;

use crate::{FuzzCase, Rng};

/// A corpus case and the coverage features it hit.
#[derive(Debug, Clone)]
pub struct CorpusEntry {
    /// The case.
    pub case: FuzzCase,
    /// Feature ids the case hit.
    pub features: Vec<u32>,
    /// Times the case was picked as a mutation parent.
    pub picks: u64,
}

/// Cases kept for adding coverage, with global feature statistics.
///
/// A feature is a coverage name from the reference adapter together with the bucket of its hit
/// count within one case (1, 2, 3, 4-7, 8-15, 16-31, 32-127, 128+), so a case that runs a loop
/// more times than any before counts as new. A case is kept when it hits a feature no earlier
/// case hit.
///
/// Parents are picked with probability proportional to their energy: the sum over their
/// features of `1 / cases that hit the feature`, damped by how often the case was already
/// picked. Cases that are among the few to reach a rare behavior get most of the mutations.
#[derive(Debug, Default)]
pub struct Corpus {
    /// Directory the cases are saved to, one JSON file each; `None` keeps them in memory.
    pub dir: Option<PathBuf>,
    /// Kept cases.
    pub entries: Vec<CorpusEntry>,
    /// Feature (`name#bucket`) to id.
    pub ids: HashMap<String, u32>,
    /// Cases that hit each feature id.
    pub hits: Vec<u64>,
    /// Distinct feature names, ignoring hit-count buckets.
    pub names: HashSet<String>,
    /// Pick weights, rebuilt when stale.
    pub weights: Vec<f64>,
    /// Picks since the weights were rebuilt.
    pub stale: u64,
}

impl Corpus {
    /// Rebuild weights after this many picks.
    pub const REWEIGHT_EVERY: u64 = 32;

    /// Opens a corpus saved in `dir`, creating the directory. Returns the corpus (empty: saved
    /// cases must be re-run to learn their features) and the saved cases.
    pub fn open(dir: Option<PathBuf>) -> eyre::Result<(Self, Vec<FuzzCase>)> {
        let mut saved = Vec::new();
        if let Some(dir) = &dir {
            fs::create_dir_all(dir)?;
            let mut paths: Vec<PathBuf> =
                fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
            paths.sort();
            for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
                let text = fs::read_to_string(path)?;
                saved.push(
                    serde_json::from_str(&text)
                        .wrap_err_with(|| format!("parsing corpus case {}", path.display()))?,
                );
            }
        }
        Ok((Self { dir, ..Default::default() }, saved))
    }

    /// The hit-count bucket of a feature count.
    pub const fn bucket(count: u64) -> u8 {
        match count {
            0..=3 => count as u8,
            4..=7 => 4,
            8..=15 => 5,
            16..=31 => 6,
            32..=127 => 7,
            _ => 8,
        }
    }

    /// Records one executed case's coverage. Returns its feature ids and how many of them no
    /// earlier case hit.
    pub fn observe(&mut self, coverage: &BTreeMap<String, u64>) -> (Vec<u32>, usize) {
        let mut features = Vec::with_capacity(coverage.len());
        let mut novel = 0;
        for (name, count) in coverage.iter().filter(|(_, c)| **c > 0) {
            let key = format!("{name}#{}", Self::bucket(*count));
            let next = self.ids.len() as u32;
            let id = *self.ids.entry(key).or_insert(next);
            if id == next {
                self.hits.push(0);
                novel += 1;
                if !self.names.contains(name) {
                    self.names.insert(name.clone());
                }
            }
            self.hits[id as usize] += 1;
            features.push(id);
        }
        (features, novel)
    }

    /// Keeps a case, saving it to the corpus directory.
    pub fn add(&mut self, case: FuzzCase, features: Vec<u32>) -> eyre::Result<()> {
        if let Some(dir) = &self.dir {
            fs::write(dir.join(format!("{}.json", case.id())), serde_json::to_vec(&case)?)?;
        }
        self.entries.push(CorpusEntry { case, features, picks: 0 });
        self.stale = Self::REWEIGHT_EVERY;
        Ok(())
    }

    /// Energy of an entry.
    pub fn energy(&self, entry: &CorpusEntry) -> f64 {
        let rarity: f64 =
            entry.features.iter().map(|id| 1.0 / self.hits[*id as usize] as f64).sum();
        rarity / (1.0 + entry.picks as f64 / 8.0).sqrt()
    }

    /// Picks a mutation parent by energy, or `None` when the corpus is empty.
    pub fn pick(&mut self, rng: &mut Rng) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        if self.stale >= Self::REWEIGHT_EVERY || self.weights.len() != self.entries.len() {
            self.weights = self.entries.iter().map(|e| self.energy(e)).collect();
            self.stale = 0;
        }
        self.stale += 1;
        let total: f64 = self.weights.iter().sum();
        let mut roll = (rng.next_u64() as f64 / u64::MAX as f64) * total;
        let mut chosen = self.entries.len() - 1;
        for (index, weight) in self.weights.iter().enumerate() {
            if roll < *weight {
                chosen = index;
                break;
            }
            roll -= weight;
        }
        self.entries[chosen].picks += 1;
        Some(chosen)
    }

    /// A splice donor for `case`: another entry on the same pre-state when there is one (half
    /// the time), otherwise any entry.
    pub fn donor(&self, rng: &mut Rng, case: &FuzzCase) -> Option<FuzzCase> {
        if self.entries.is_empty() {
            return None;
        }
        if rng.chance(50) {
            let same: Vec<&CorpusEntry> = self
                .entries
                .iter()
                .filter(|e| {
                    e.case.seed == case.seed && e.case.index == case.index && e.case != *case
                })
                .collect();
            if !same.is_empty() {
                return Some(same[rng.index(same.len())].case.clone());
            }
        }
        Some(self.entries[rng.index(self.entries.len())].case.clone())
    }

    /// Distinct feature names per family (the part before the first `:`).
    pub fn families(&self) -> BTreeMap<String, u64> {
        let mut families = BTreeMap::new();
        for name in &self.names {
            let family = name.split(':').next().unwrap_or_default();
            *families.entry(family.to_owned()).or_default() += 1;
        }
        families
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::Corpus;

    #[test]
    fn new_names_and_new_hit_count_buckets_are_novel() {
        let mut corpus = Corpus::default();
        let coverage = |pairs: &[(&str, u64)]| -> BTreeMap<String, u64> {
            pairs.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect()
        };
        assert_eq!(corpus.observe(&coverage(&[("op:a", 1), ("op:b", 2)])).1, 2);
        assert_eq!(corpus.observe(&coverage(&[("op:a", 1), ("op:b", 2)])).1, 0);
        // Same names, but `op:a` ran often enough to land in a new bucket.
        assert_eq!(corpus.observe(&coverage(&[("op:a", 9), ("op:b", 3)])).1, 2);
        assert_eq!(corpus.names.len(), 2);
    }
}
