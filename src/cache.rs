use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::TidalTrackCandidate;

const CACHE_SCHEMA_VERSION: u8 = 1;
const DEFAULT_CACHE_PATH: &str = "data/tidal-search-cache.jsonl";

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    schema_version: u8,
    country_code: String,
    query: String,
    cached_at_unix: u64,
    candidates: Vec<TidalTrackCandidate>,
}

pub struct TidalSearchCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, Vec<TidalTrackCandidate>>>,
}

impl TidalSearchCache {
    pub fn load_default() -> Result<Self> {
        Self::load(PathBuf::from(DEFAULT_CACHE_PATH))
    }

    fn load(path: PathBuf) -> Result<Self> {
        let mut entries = HashMap::new();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not read TIDAL cache {}", path.display()));
            }
        };

        let lines: Vec<&str> = contents.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let entry: CacheEntry = match serde_json::from_str(line) {
                Ok(entry) => entry,
                Err(error) if index + 1 == lines.len() && !contents.ends_with('\n') => {
                    eprintln!(
                        "Removing an incomplete final line from TIDAL cache {}: {error}",
                        path.display()
                    );
                    let valid_length = contents.rfind('\n').map_or(0, |position| position + 1);
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .with_context(|| {
                            format!("Could not repair TIDAL cache {}", path.display())
                        })?
                        .set_len(valid_length as u64)
                        .with_context(|| {
                            format!("Could not truncate TIDAL cache {}", path.display())
                        })?;
                    continue;
                }
                Err(error) => {
                    bail!(
                        "Invalid TIDAL cache entry at {}:{}: {error}",
                        path.display(),
                        index + 1
                    );
                }
            };

            if entry.schema_version != CACHE_SCHEMA_VERSION {
                continue;
            }

            entries.insert(
                cache_key(&entry.country_code, &entry.query),
                entry.candidates,
            );
        }

        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.lock_entries()?.len())
    }

    pub fn get(&self, country_code: &str, query: &str) -> Result<Option<Vec<TidalTrackCandidate>>> {
        Ok(self
            .lock_entries()?
            .get(&cache_key(country_code, query))
            .cloned())
    }

    pub fn insert(
        &self,
        country_code: &str,
        query: &str,
        candidates: &[TidalTrackCandidate],
        cached_at_unix: u64,
    ) -> Result<bool> {
        let key = cache_key(country_code, query);
        let mut entries = self.lock_entries()?;
        if entries.contains_key(&key) {
            return Ok(false);
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let entry = CacheEntry {
            schema_version: CACHE_SCHEMA_VERSION,
            country_code: country_code.to_owned(),
            query: query.to_owned(),
            cached_at_unix,
            candidates: candidates.to_vec(),
        };
        let mut serialized = serde_json::to_vec(&entry)?;
        serialized.push(b'\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Could not open TIDAL cache {}", self.path.display()))?;
        file.write_all(&serialized)
            .with_context(|| format!("Could not update TIDAL cache {}", self.path.display()))?;
        file.flush()
            .with_context(|| format!("Could not flush TIDAL cache {}", self.path.display()))?;

        entries.insert(key, candidates.to_vec());
        Ok(true)
    }

    fn lock_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<TidalTrackCandidate>>>> {
        self.entries
            .lock()
            .map_err(|_| anyhow::anyhow!("TIDAL cache lock was poisoned"))
    }
}

fn cache_key(country_code: &str, query: &str) -> String {
    format!("{}\u{1f}{query}", country_code.trim().to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::TidalSearchCache;
    use crate::model::TidalTrackCandidate;

    #[test]
    fn persists_and_reloads_candidates_by_country_and_query() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tidal-cache-{unique}.jsonl"));
        let candidate = TidalTrackCandidate {
            tidal_id: "123".to_owned(),
            title: "Canción".to_owned(),
            version: None,
            isrc: Some("PEABC2600001".to_owned()),
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
        };

        let cache = TidalSearchCache::load(path.clone()).unwrap();
        assert!(cache.get("PE", "Canción Artista").unwrap().is_none());
        assert!(
            cache
                .insert("PE", "Canción Artista", std::slice::from_ref(&candidate), 1)
                .unwrap()
        );
        assert!(
            !cache
                .insert("PE", "Canción Artista", std::slice::from_ref(&candidate), 2)
                .unwrap()
        );

        let reloaded = TidalSearchCache::load(path.clone()).unwrap();
        let candidates = reloaded.get("pe", "Canción Artista").unwrap().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].tidal_id, "123");
        assert!(reloaded.get("US", "Canción Artista").unwrap().is_none());

        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"incomplete\"")
            .unwrap();
        let repaired = TidalSearchCache::load(path.clone()).unwrap();
        assert_eq!(repaired.len().unwrap(), 1);
        assert!(repaired.insert("US", "Another Query", &[], 3).unwrap());
        assert_eq!(
            TidalSearchCache::load(path.clone()).unwrap().len().unwrap(),
            2
        );

        std::fs::remove_file(path).unwrap();
    }
}
