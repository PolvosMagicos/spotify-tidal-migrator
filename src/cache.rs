use std::{
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::TidalTrackCandidate;

const CACHE_SCHEMA_VERSION: u8 = 1;
const DEFAULT_CACHE_PATH: &str = "data/tidal-search-cache.jsonl";
const DEFAULT_SUCCESS_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_EMPTY_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    schema_version: u8,
    country_code: String,
    query: String,
    cached_at_unix: u64,
    candidates: Vec<TidalTrackCandidate>,
}

#[derive(Debug)]
struct CachedSearch {
    cached_at_unix: u64,
    candidates: Vec<TidalTrackCandidate>,
}

pub struct TidalSearchCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, CachedSearch>>,
    success_ttl_secs: u64,
    empty_ttl_secs: u64,
}

impl TidalSearchCache {
    pub fn load_default() -> Result<Self> {
        Self::load_with_ttls(
            PathBuf::from(DEFAULT_CACHE_PATH),
            ttl_from_env("TIDAL_CACHE_TTL_SECS", DEFAULT_SUCCESS_TTL_SECS)?,
            ttl_from_env("TIDAL_NEGATIVE_CACHE_TTL_SECS", DEFAULT_EMPTY_TTL_SECS)?,
        )
    }

    fn load_with_ttls(path: PathBuf, success_ttl_secs: u64, empty_ttl_secs: u64) -> Result<Self> {
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
                CachedSearch {
                    cached_at_unix: entry.cached_at_unix,
                    candidates: entry.candidates,
                },
            );
        }

        let cache = Self {
            path,
            entries: Mutex::new(entries),
            success_ttl_secs,
            empty_ttl_secs,
        };
        cache.remove_expired(current_unix_timestamp()?)?;
        Ok(cache)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> Result<usize> {
        self.remove_expired(current_unix_timestamp()?)?;
        Ok(self.lock_entries()?.len())
    }

    pub fn get(&self, country_code: &str, query: &str) -> Result<Option<Vec<TidalTrackCandidate>>> {
        self.get_at(country_code, query, current_unix_timestamp()?)
    }

    pub fn insert(
        &self,
        country_code: &str,
        query: &str,
        candidates: &[TidalTrackCandidate],
        cached_at_unix: u64,
    ) -> Result<bool> {
        self.store(country_code, query, candidates, cached_at_unix, false)
    }

    pub fn replace(
        &self,
        country_code: &str,
        query: &str,
        candidates: &[TidalTrackCandidate],
        cached_at_unix: u64,
    ) -> Result<()> {
        self.store(country_code, query, candidates, cached_at_unix, true)?;
        Ok(())
    }

    fn get_at(
        &self,
        country_code: &str,
        query: &str,
        now_unix: u64,
    ) -> Result<Option<Vec<TidalTrackCandidate>>> {
        let key = cache_key(country_code, query);
        let mut entries = self.lock_entries()?;
        let Some(entry) = entries.get(&key) else {
            return Ok(None);
        };

        if is_fresh(entry, now_unix, self.success_ttl_secs, self.empty_ttl_secs) {
            return Ok(Some(entry.candidates.clone()));
        }

        entries.remove(&key);
        Ok(None)
    }

    fn store(
        &self,
        country_code: &str,
        query: &str,
        candidates: &[TidalTrackCandidate],
        cached_at_unix: u64,
        replace_existing: bool,
    ) -> Result<bool> {
        let key = cache_key(country_code, query);
        let mut entries = self.lock_entries()?;
        if !replace_existing && entries.contains_key(&key) {
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

        entries.insert(
            key,
            CachedSearch {
                cached_at_unix,
                candidates: candidates.to_vec(),
            },
        );
        Ok(true)
    }

    fn remove_expired(&self, now_unix: u64) -> Result<()> {
        let mut entries = self.lock_entries()?;
        entries.retain(|_, entry| {
            is_fresh(entry, now_unix, self.success_ttl_secs, self.empty_ttl_secs)
        });
        Ok(())
    }

    fn lock_entries(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, CachedSearch>>> {
        self.entries
            .lock()
            .map_err(|_| anyhow::anyhow!("TIDAL cache lock was poisoned"))
    }
}

fn is_fresh(
    entry: &CachedSearch,
    now_unix: u64,
    success_ttl_secs: u64,
    empty_ttl_secs: u64,
) -> bool {
    let ttl = if entry.candidates.is_empty() {
        empty_ttl_secs
    } else {
        success_ttl_secs
    };
    now_unix.saturating_sub(entry.cached_at_unix) < ttl
}

fn ttl_from_env(name: &str, default: u64) -> Result<u64> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value.to_string_lossy();
    value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a non-negative number of seconds"))
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_secs())
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

        let cache = TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX).unwrap();
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

        let reloaded = TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX).unwrap();
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
        let repaired = TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX).unwrap();
        assert_eq!(repaired.len().unwrap(), 1);
        assert!(repaired.insert("US", "Another Query", &[], 3).unwrap());
        assert_eq!(
            TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX)
                .unwrap()
                .len()
                .unwrap(),
            2
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn expires_empty_results_before_successful_results() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tidal-cache-expiry-{unique}.jsonl"));
        let candidate = TidalTrackCandidate {
            tidal_id: "123".to_owned(),
            title: "Canción".to_owned(),
            version: None,
            isrc: None,
            duration_ms: None,
            explicit: None,
            artists: Vec::new(),
            album: None,
        };
        let cache = TidalSearchCache::load_with_ttls(path.clone(), 100, 10).unwrap();
        cache
            .insert("PE", "Found", std::slice::from_ref(&candidate), 1_000)
            .unwrap();
        cache.insert("PE", "Empty", &[], 1_000).unwrap();

        assert!(cache.get_at("PE", "Found", 1_011).unwrap().is_some());
        assert!(cache.get_at("PE", "Empty", 1_011).unwrap().is_none());
        assert!(cache.get_at("PE", "Found", 1_101).unwrap().is_none());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refresh_replaces_an_existing_entry() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tidal-cache-refresh-{unique}.jsonl"));
        let cache = TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX).unwrap();
        cache.insert("PE", "Query", &[], 1).unwrap();

        let candidate = TidalTrackCandidate {
            tidal_id: "replacement".to_owned(),
            title: "Found".to_owned(),
            version: None,
            isrc: None,
            duration_ms: None,
            explicit: None,
            artists: Vec::new(),
            album: None,
        };
        cache
            .replace("PE", "Query", std::slice::from_ref(&candidate), 2)
            .unwrap();

        assert_eq!(
            cache.get_at("PE", "Query", 3).unwrap().unwrap()[0].tidal_id,
            "replacement"
        );
        assert_eq!(
            TidalSearchCache::load_with_ttls(path.clone(), u64::MAX, u64::MAX)
                .unwrap()
                .get_at("PE", "Query", 3)
                .unwrap()
                .unwrap()[0]
                .tidal_id,
            "replacement"
        );

        std::fs::remove_file(path).unwrap();
    }
}
