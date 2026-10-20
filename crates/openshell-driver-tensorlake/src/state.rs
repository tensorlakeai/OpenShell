// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! On-disk copy of each sandbox request.
//!
//! Tensorlake keeps no labels, so a restarted driver can find its VMs by
//! name but not the `OpenShell` name, namespace, and spec the gateway gave
//! them. The gateway copies the name from every driver snapshot into its own
//! record and refuses a name change, so a recovered record without the name
//! can never update its status. This store keeps the request so recovery
//! can report it as it was.
//!
//! Files hold no secrets. Save clears the sandbox token and the launch
//! authentication, and the directory and files are owner-only.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use openshell_core::proto::compute::v1::DriverSandbox;
use prost::Message as _;

const SANDBOXES_DIR: &str = "sandboxes";
const FILE_SUFFIX: &str = ".pb";

#[derive(Debug, Clone)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    /// A store under `dir`. The directory is created on the first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.dir.join(SANDBOXES_DIR)
    }

    fn path(&self, sandbox_id: &str) -> PathBuf {
        self.sandboxes_dir()
            .join(format!("{}{FILE_SUFFIX}", file_stem(sandbox_id)))
    }

    /// Keep the request without its secrets.
    pub async fn save(&self, sandbox: &DriverSandbox) -> Result<(), String> {
        let mut stored = sandbox.clone();
        if let Some(spec) = stored.spec.as_mut() {
            spec.sandbox_token.clear();
            spec.launch_authentication.clear();
        }
        let dir = self.sandboxes_dir();
        ensure_private_dir(&dir)
            .await
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        let destination = self.path(&sandbox.id);
        let temporary = dir.join(format!(
            ".{}.{}.tmp",
            file_stem(&sandbox.id),
            std::process::id()
        ));
        let result = write_private_file(&temporary, stored.encode_to_vec()).await;
        let result = match result {
            Ok(()) => tokio::fs::rename(&temporary, &destination).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(format!("write {}: {error}", destination.display()));
        }
        Ok(())
    }

    /// The saved request, or `None` when there is no file for this ID.
    pub async fn load(&self, sandbox_id: &str) -> Result<Option<DriverSandbox>, String> {
        let path = self.path(sandbox_id);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        DriverSandbox::decode(bytes.as_slice())
            .map(Some)
            .map_err(|error| format!("decode {}: {error}", path.display()))
    }

    /// IDs of every saved request. An absent directory holds none.
    pub async fn saved_ids(&self) -> Result<Vec<String>, String> {
        let dir = self.sandboxes_dir();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("read {}: {error}", dir.display())),
        };
        let mut ids = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| format!("read {}: {error}", dir.display()))?
        {
            let name = entry.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(FILE_SUFFIX))
            {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Delete the saved request. A missing file is not an error.
    pub async fn remove(&self, sandbox_id: &str) -> Result<(), String> {
        let path = self.path(sandbox_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        }
    }
}

/// A file name for a sandbox ID. IDs are UUIDs, but keep every other
/// character out of the path anyway.
fn file_stem(sandbox_id: &str) -> String {
    sandbox_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

async fn ensure_private_dir(dir: &Path) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await
}

async fn write_private_file(path: &Path, bytes: Vec<u8>) -> Result<(), std::io::Error> {
    tokio::fs::write(path, bytes).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use openshell_core::proto::compute::v1::DriverSandboxSpec;

    fn temp_store(label: &str) -> StateStore {
        let dir = std::env::temp_dir().join(format!(
            "openshell-tensorlake-state-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        StateStore::new(dir)
    }

    #[tokio::test]
    async fn save_load_remove_round_trip() {
        let store = temp_store("round-trip");
        let sandbox = DriverSandbox {
            id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
            name: "demo".into(),
            namespace: "default".into(),
            workspace: "ws".into(),
            spec: Some(DriverSandboxSpec {
                log_level: "debug".into(),
                sandbox_token: "secret-token".into(),
                launch_authentication: b"{\"secret\":true}".to_vec(),
                ..DriverSandboxSpec::default()
            }),
            ..DriverSandbox::default()
        };

        assert_eq!(store.load(&sandbox.id).await.unwrap(), None);
        assert!(store.saved_ids().await.unwrap().is_empty());
        store.save(&sandbox).await.unwrap();

        let loaded = store.load(&sandbox.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, sandbox.id);
        assert_eq!(loaded.name, "demo");
        assert_eq!(loaded.namespace, "default");
        assert_eq!(loaded.workspace, "ws");
        let spec = loaded.spec.unwrap();
        assert_eq!(spec.log_level, "debug");
        assert!(spec.sandbox_token.is_empty());
        assert!(spec.launch_authentication.is_empty());

        let path = store.path(&sandbox.id);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(store.sandboxes_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert!(
            !std::fs::read(&path)
                .unwrap()
                .windows(12)
                .any(|window| window == b"secret-token")
        );

        assert_eq!(store.saved_ids().await.unwrap(), vec![sandbox.id.clone()]);
        store.remove(&sandbox.id).await.unwrap();
        assert_eq!(store.load(&sandbox.id).await.unwrap(), None);
        assert!(store.saved_ids().await.unwrap().is_empty());
        // Removing twice is fine.
        store.remove(&sandbox.id).await.unwrap();
    }

    #[test]
    fn file_stem_keeps_ids_inside_the_directory() {
        let store = StateStore::new("/state");
        assert_eq!(
            store.path("../etc/passwd"),
            PathBuf::from("/state/sandboxes/---etc-passwd.pb")
        );
        assert_eq!(
            store.path("aaaaaaaa-0000-4000-8000-000000000001"),
            PathBuf::from("/state/sandboxes/aaaaaaaa-0000-4000-8000-000000000001.pb")
        );
    }
}
