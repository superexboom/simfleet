// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional check-native-only WASM results, shared by CPU variant checks.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Default, Serialize, Deserialize)]
struct Entries {
    fingerprint: String,
    results: BTreeMap<String, Entry>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    checksum: String,
    answer: Value,
}
struct Cache {
    path: PathBuf,
    data: Entries,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
pub fn init(path: &Path, wasm: &[u8], data: Option<&[u8]>, engine: &str) -> Result<()> {
    let exe = std::fs::read(std::env::current_exe()?)?;
    let key = serde_json::json!({"format":1,"engine":engine,"wasm":digest(wasm),"data":digest(data.unwrap_or_default()),"checker":digest(&exe)});
    let fingerprint = digest(&serde_json::to_vec(&key)?);
    let loaded = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Entries>(&b).ok())
        .filter(|c| c.fingerprint == fingerprint)
        .unwrap_or_else(|| Entries {
            fingerprint,
            results: BTreeMap::new(),
        });
    CACHE
        .set(Mutex::new(Cache {
            path: path.into(),
            data: loaded,
        }))
        .map_err(|_| anyhow::anyhow!("reference cache already configured"))?;
    Ok(())
}
fn valid(entry: &Entry) -> bool {
    serde_json::to_vec(&entry.answer).is_ok_and(|b| digest(&b) == entry.checksum)
}
pub fn compute(key: &Value, run: impl FnOnce() -> Result<Value>) -> Result<Value> {
    let Some(cache) = CACHE.get() else {
        return run();
    };
    let key = digest(&serde_json::to_vec(key)?);
    if let Some(answer) = cache
        .lock()
        .unwrap()
        .data
        .results
        .get(&key)
        .filter(|e| valid(e))
        .map(|e| e.answer.clone())
    {
        println!("REFERENCE-CACHE hit {}", &key[..12]);
        return Ok(answer);
    }
    let answer = run()?;
    let mut c = cache.lock().unwrap();
    c.data.results.insert(
        key.clone(),
        Entry {
            checksum: digest(&serde_json::to_vec(&answer)?),
            answer: answer.clone(),
        },
    );
    if let Some(parent) = c.path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = c.path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&c.data)?)?;
    std::fs::rename(tmp, &c.path)?;
    println!("REFERENCE-CACHE computed {}", &key[..12]);
    Ok(answer)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn altered_reference_answer_is_not_reused() {
        let a = serde_json::json!({"work":123,"score":1.25});
        let mut entry = Entry {
            checksum: digest(&serde_json::to_vec(&a).unwrap()),
            answer: a,
        };
        assert!(valid(&entry));
        entry.answer["work"] = serde_json::json!(124);
        assert!(!valid(&entry));
    }
}
