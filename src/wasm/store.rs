//! Durable storage for hook artifacts and the script index.
//!
//! ```text
//! <data_dir>/artifacts/ab/ab12…ef.wasm    immutable, content-addressed (sha256)
//! <data_dir>/artifacts/ab/ab12…ef.cwasm   ahead-of-time compiled form (rebuilt if the engine changes)
//! <data_dir>/scripts/index.json           scripts → versions → digests, aliases
//! ```
//!
//! A *script* is a named unit with a unique id; every upload becomes an immutable numbered
//! *version*. Routes pin a version (`wasm:name@3`), a digest (`wasm:name@sha256:ab12…`) or a
//! moving alias (`wasm:name@live`). References are resolved to a digest when a runtime is
//! built, so the request path never consults the store.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where a version came from when it was built from source by the builder service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    /// `sha256:…` of the source text; the text itself is in `sources/`.
    pub digest: String,
    /// typescript | javascript | python
    pub language: String,
    pub backend: Option<String>,
    /// Compiler identification reported by the builder.
    pub toolchain: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub version: u32,
    /// `sha256:<64 hex>`
    pub digest: String,
    pub size: u64,
    pub language: String,
    pub note: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Present when built from source (absent for direct component uploads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    /// Stable unique id (`scr_…`); the name is the human handle.
    pub id: String,
    pub created_at: u64,
    pub versions: Vec<Version>,
    pub aliases: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Index {
    scripts: BTreeMap<String, Script>,
}

/// A script reference resolved to a concrete, immutable artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub name: String,
    pub script_id: String,
    pub version: u32,
    pub digest: String,
    /// Set when the reference went through an alias.
    pub alias: Option<String>,
}

pub struct ScriptStore {
    root: PathBuf,
    index: Mutex<Index>,
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Accepts only `sha256:<64 lowercase hex>`; the hex is what ends up in file paths.
fn digest_hex(d: &str) -> Result<&str> {
    let h = d.strip_prefix("sha256:").ok_or_else(|| anyhow!("bad digest `{d}`"))?;
    if h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(h)
    } else {
        bail!("bad digest `{d}`")
    }
}

impl ScriptStore {
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(root.join("artifacts"))?;
        std::fs::create_dir_all(root.join("scripts"))?;
        std::fs::create_dir_all(root.join("sources"))?;
        let idx_path = root.join("scripts/index.json");
        let index = match std::fs::read_to_string(&idx_path) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("corrupt script index {}", idx_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", idx_path.display())),
        };
        Ok(Arc::new(Self { root: root.to_path_buf(), index: Mutex::new(index) }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, idx: &Index) -> Result<()> {
        let path = self.root.join("scripts/index.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(idx)?)?;
        std::fs::rename(&tmp, &path).context("replacing script index")
    }

    pub fn wasm_path(&self, digest: &str) -> Result<PathBuf> {
        let h = digest_hex(digest)?;
        Ok(self.root.join("artifacts").join(&h[..2]).join(format!("{h}.wasm")))
    }

    pub fn cwasm_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.wasm_path(digest)?.with_extension("cwasm"))
    }

    /// Store raw component bytes (idempotent). Returns the digest.
    pub fn put_artifact(&self, bytes: &[u8]) -> Result<String> {
        let digest = sha256_digest(bytes);
        let path = self.wasm_path(&digest)?;
        if !path.exists() {
            std::fs::create_dir_all(path.parent().expect("artifact path has a parent"))?;
            let tmp = path.with_extension("wasm.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(digest)
    }

    /// Store source text (content-addressed). Returns `sha256:…`.
    pub fn put_source(&self, text: &str) -> Result<String> {
        let digest = sha256_digest(text.as_bytes());
        let path = self.source_path(&digest)?;
        if !path.exists() {
            let tmp = path.with_extension("src.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(digest)
    }

    pub fn read_source(&self, digest: &str) -> Result<String> {
        std::fs::read_to_string(self.source_path(digest)?).with_context(|| format!("source {digest} is missing from the store"))
    }

    fn source_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("sources").join(format!("{}.src", digest_hex(digest)?)))
    }

    /// An existing version of `name` built from exactly this source with the same settings.
    pub fn find_by_source(&self, name: &str, source_digest: &str, language: &str, backend: Option<&str>) -> Option<Version> {
        self.lock().scripts.get(name)?.versions.iter().find(|v| {
            v.source.as_ref().is_some_and(|s| s.digest == source_digest && s.language == language && s.backend.as_deref() == backend)
        }).cloned()
    }

    pub fn read_artifact(&self, digest: &str) -> Result<Vec<u8>> {
        let p = self.wasm_path(digest)?;
        std::fs::read(&p).with_context(|| format!("artifact {digest} is missing from the store"))
    }

    /// Register `digest` as the next version of `name` (creating the script if needed).
    /// Uploading bytes identical to an existing version returns that version (`false`).
    pub fn add_version(&self, name: &str, digest: &str, size: u64, language: &str, note: &str, source: Option<SourceInfo>) -> Result<(Version, bool)> {
        if !valid_name(name) {
            bail!("script name must match [a-z0-9][a-z0-9_-]{{0,62}}");
        }
        digest_hex(digest)?;
        let mut idx = self.lock();
        let script = idx.scripts.entry(name.to_string()).or_insert_with(|| Script {
            id: format!("scr_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            created_at: now(),
            versions: vec![],
            aliases: BTreeMap::new(),
        });
        if let Some(v) = script.versions.iter().find(|v| v.digest == digest) {
            return Ok((v.clone(), false));
        }
        let v = Version {
            version: script.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1,
            digest: digest.to_string(),
            size,
            language: language.to_string(),
            note: note.to_string(),
            created_at: now(),
            source,
        };
        script.versions.push(v.clone());
        let snapshot = idx.clone();
        self.save(&snapshot)?;
        Ok((v, true))
    }

    pub fn set_alias(&self, name: &str, alias: &str, version: u32) -> Result<()> {
        if !valid_name(alias) {
            bail!("alias must match [a-z0-9][a-z0-9_-]{{0,62}}");
        }
        let mut idx = self.lock();
        let s = idx.scripts.get_mut(name).ok_or_else(|| anyhow!("no such script `{name}`"))?;
        if !s.versions.iter().any(|v| v.version == version) {
            bail!("script `{name}` has no version {version}");
        }
        s.aliases.insert(alias.to_string(), version);
        let snapshot = idx.clone();
        self.save(&snapshot)
    }

    pub fn delete_alias(&self, name: &str, alias: &str) -> Result<()> {
        let mut idx = self.lock();
        let s = idx.scripts.get_mut(name).ok_or_else(|| anyhow!("no such script `{name}`"))?;
        s.aliases.remove(alias).ok_or_else(|| anyhow!("script `{name}` has no alias `{alias}`"))?;
        let snapshot = idx.clone();
        self.save(&snapshot)
    }

    /// Remove a version. Fails if an alias still points at it (move the alias first).
    pub fn delete_version(&self, name: &str, version: u32) -> Result<Version> {
        let mut idx = self.lock();
        let s = idx.scripts.get_mut(name).ok_or_else(|| anyhow!("no such script `{name}`"))?;
        if let Some((alias, _)) = s.aliases.iter().find(|(_, v)| **v == version) {
            bail!("alias `{alias}` still points at version {version}");
        }
        let pos = s.versions.iter().position(|v| v.version == version).ok_or_else(|| anyhow!("no such version"))?;
        let removed = s.versions.remove(pos);
        let still_used = idx.scripts.values().flat_map(|s| &s.versions).any(|v| v.digest == removed.digest);
        let snapshot = idx.clone();
        self.save(&snapshot)?;
        drop(idx);
        if let Some(src) = &removed.source {
            let shared = self.lock().scripts.values().flat_map(|s| &s.versions).any(|v| v.source.as_ref().is_some_and(|x| x.digest == src.digest));
            if !shared {
                if let Ok(p) = self.source_path(&src.digest) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        if !still_used {
            // best effort: reclaim the artifact and its compiled form
            for p in [self.wasm_path(&removed.digest), self.cwasm_path(&removed.digest)].into_iter().flatten() {
                let _ = std::fs::remove_file(p);
            }
        }
        Ok(removed)
    }

    pub fn list(&self) -> BTreeMap<String, Script> {
        self.lock().scripts.clone()
    }

    pub fn get(&self, name: &str) -> Option<Script> {
        self.lock().scripts.get(name).cloned()
    }

    /// `3` → version, `sha256:ab12…` → digest prefix (≥12 hex), anything else → alias.
    pub fn resolve(&self, name: &str, reference: &str) -> Result<Resolved> {
        let idx = self.lock();
        let s = idx.scripts.get(name).ok_or_else(|| anyhow!("no such script `{name}`"))?;
        let (v, alias) = if let Ok(n) = reference.parse::<u32>() {
            (s.versions.iter().find(|v| v.version == n), None)
        } else if let Some(prefix) = reference.strip_prefix("sha256:") {
            if prefix.len() < 12 {
                bail!("digest prefix in `{name}@{reference}` must be at least 12 hex characters");
            }
            let m: Vec<_> = s.versions.iter().filter(|v| v.digest["sha256:".len()..].starts_with(prefix)).collect();
            if m.len() > 1 {
                bail!("digest prefix `{prefix}` is ambiguous in script `{name}`");
            }
            (m.first().copied(), None)
        } else {
            let ver = s.aliases.get(reference).ok_or_else(|| {
                anyhow!("script `{name}` has no alias `{reference}` (aliases: {:?})", s.aliases.keys().collect::<Vec<_>>())
            })?;
            (s.versions.iter().find(|v| v.version == *ver), Some(reference.to_string()))
        };
        let v = v.ok_or_else(|| anyhow!("script `{name}` has no version matching `{reference}`"))?;
        Ok(Resolved { name: name.into(), script_id: s.id.clone(), version: v.version, digest: v.digest.clone(), alias })
    }
}

/// `wasm:name@ref` → `(name, ref)`. A bare `wasm:name` means `@live`.
pub fn parse_ref(hook: &str) -> Option<(&str, &str)> {
    let rest = hook.strip_prefix("wasm:")?;
    Some(rest.split_once('@').unwrap_or((rest, "live")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_aliases_and_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let s = ScriptStore::open(dir.path()).unwrap();
        let d1 = s.put_artifact(b"one").unwrap();
        let d2 = s.put_artifact(b"two").unwrap();
        let (v1, created) = s.add_version("pets", &d1, 3, "js", "first", None).unwrap();
        assert!(created && v1.version == 1);
        assert!(!s.add_version("pets", &d1, 3, "js", "again", None).unwrap().1, "same bytes -> same version");
        assert_eq!(s.add_version("pets", &d2, 3, "py", "", None).unwrap().0.version, 2);

        s.set_alias("pets", "live", 1).unwrap();
        let r = s.resolve("pets", "live").unwrap();
        assert_eq!((r.version, r.alias.as_deref()), (1, Some("live")));
        s.set_alias("pets", "live", 2).unwrap();
        assert_eq!(s.resolve("pets", "live").unwrap().digest, d2, "alias moved");
        assert_eq!(s.resolve("pets", "1").unwrap().digest, d1, "numeric pin is immutable");
        assert_eq!(s.resolve("pets", &d1[..19]).unwrap().version, 1, "digest prefix");
        assert!(s.resolve("pets", "sha256:abc").is_err(), "prefix too short");
        assert!(s.resolve("pets", "9").is_err());
        assert!(s.resolve("pets", "nope").unwrap_err().to_string().contains("no alias"));
        assert!(s.resolve("missing", "1").is_err());

        assert!(s.delete_version("pets", 2).is_err(), "alias still points at v2");
        s.delete_version("pets", 1).unwrap();
        assert!(!s.wasm_path(&d1).unwrap().exists(), "unreferenced artifact reclaimed");

        // survives a restart
        let again = ScriptStore::open(dir.path()).unwrap();
        assert_eq!(again.get("pets").unwrap().versions.len(), 1);
        assert_eq!(again.resolve("pets", "live").unwrap().version, 2);
    }

    #[test]
    fn source_is_stored_deduplicated_and_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let s = ScriptStore::open(dir.path()).unwrap();
        let sd = s.put_source("export const handler = {};").unwrap();
        assert_eq!(s.put_source("export const handler = {};").unwrap(), sd, "content-addressed");
        assert_eq!(s.read_source(&sd).unwrap(), "export const handler = {};");
        let d1 = s.put_artifact(b"wasm-one").unwrap();
        let info = SourceInfo { digest: sd.clone(), language: "typescript".into(), backend: None, toolchain: "jco".into() };
        let (v, _) = s.add_version("p", &d1, 8, "typescript", "", Some(info)).unwrap();
        assert_eq!(s.find_by_source("p", &sd, "typescript", None).unwrap().version, v.version);
        assert!(s.find_by_source("p", &sd, "typescript", Some("spidermonkey")).is_none(), "backend is part of the identity");
        assert!(s.find_by_source("p", &sd, "python", None).is_none());
        assert!(s.find_by_source("other", &sd, "typescript", None).is_none());
        // persisted in the index and readable after reopening
        let again = ScriptStore::open(dir.path()).unwrap();
        assert_eq!(again.get("p").unwrap().versions[0].source.as_ref().unwrap().toolchain, "jco");
        s.delete_version("p", 1).unwrap();
        assert!(s.read_source(&sd).is_err(), "unshared source file is reclaimed with its last version");
    }

    #[test]
    fn names_digests_and_refs() {
        assert!(valid_name("validate-pet_2") && !valid_name("Bad") && !valid_name("-x") && !valid_name("a/b") && !valid_name(""));
        assert!(digest_hex("sha256:../../etc/passwd").is_err());
        assert_eq!(parse_ref("wasm:pets@3"), Some(("pets", "3")));
        assert_eq!(parse_ref("wasm:pets"), Some(("pets", "live")));
        assert_eq!(parse_ref("builtin:stamp"), None);
    }
}
