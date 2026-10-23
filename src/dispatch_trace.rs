// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small credential-free scheduling evidence; at most two 1 MiB files.
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
pub struct Trace {
    tx: mpsc::SyncSender<Value>,
}
impl Trace {
    pub fn new(dir: &Path) -> Self {
        let path = dir.join("dispatch.log");
        let (tx, rx) = mpsc::sync_channel::<Value>(64);
        let _ = std::thread::Builder::new()
            .name("dispatch-log".into())
            .spawn(move || {
                let previous = PathBuf::from(format!("{}.1", path.display()));
                for mut v in rx {
                    v["at"] = serde_json::json!(crate::sys::now_ms());
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
                        let _ = std::fs::remove_file(&previous);
                        let _ = std::fs::rename(&path, &previous);
                    }
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        let _ = writeln!(file, "{v}");
                    }
                }
            });
        Self { tx }
    }
    pub fn event(&self, value: Value) {
        let _ = self.tx.try_send(value);
    }
}
