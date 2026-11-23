//! Profiles: independent sets of repositories, settings, commit signature and git credentials.
//!
//! Each profile has its own user database and keychain namespace; the app runs one [`Engine`] for the active
//! profile and rebuilds it on a switch. The list of profiles and the active one live in `profiles.json` in the base
//! data directory. The built-in profile ([`DEFAULT_PROFILE_ID`]) uses the base directories and the original keychain
//! service, so data from before profiles existed is the Default profile's. Every other profile lives under
//! `<data_dir>/profiles/<id>` (and `<cache_dir>/profiles/<id>`) with the keychain service `<service>.profile.<id>`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gityas_auth::{CredentialStore, KEYCHAIN_SERVICE, KeyringStore};
use gityas_core::models::OpenTabs;
use gityas_core::{GitYasError, Provider, RepoId, Result};
use gityas_platform::Paths;
use gityas_storage::{Storage, StoragePaths};
use serde::{Deserialize, Serialize};

use crate::Engine;

/// Id of the profile every installation starts with. It cannot be deleted (it can be renamed).
pub const DEFAULT_PROFILE_ID: &str = "default";
/// Longest profile name, in characters.
pub const MAX_PROFILE_NAME: usize = 40;

const REGISTRY_FILE: &str = "profiles.json";
const PROFILES_DIR: &str = "profiles";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

/// Every profile, in the order they were added (Default first), and which one is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profiles {
    pub active: String,
    pub profiles: Vec<Profile>,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            active: DEFAULT_PROFILE_ID.to_owned(),
            profiles: vec![Profile {
                id: DEFAULT_PROFILE_ID.to_owned(),
                name: "Default".to_owned(),
            }],
        }
    }
}

impl Profiles {
    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The active profile (the Default one if the registry points nowhere).
    pub fn active_profile(&self) -> &Profile {
        self.get(&self.active)
            .or_else(|| self.profiles.first())
            .unwrap_or_else(|| unreachable_default())
    }

    /// Repairs a hand-edited or damaged registry: the Default profile exists and comes first, ids are unique, names
    /// are valid, and `active` names a profile.
    fn normalized(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.profiles.retain(|p| valid_id(&p.id) && seen.insert(p.id.clone()));
        for p in &mut self.profiles {
            if clean_name(&p.name).is_err() {
                p.name = if p.id == DEFAULT_PROFILE_ID {
                    "Default".into()
                } else {
                    "Profile".into()
                };
            }
        }
        match self.profiles.iter().position(|p| p.id == DEFAULT_PROFILE_ID) {
            Some(0) => {}
            Some(i) => {
                let default = self.profiles.remove(i);
                self.profiles.insert(0, default);
            }
            None => self.profiles.insert(0, Profiles::default().profiles.remove(0)),
        }
        if self.get(&self.active).is_none() {
            self.active = DEFAULT_PROFILE_ID.to_owned();
        }
        self
    }
}

/// `Profiles::default()` always has the Default profile; this only satisfies the type checker without a panic path
/// in normal code.
fn unreachable_default() -> &'static Profile {
    static DEFAULT: std::sync::OnceLock<Profile> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| Profile {
        id: DEFAULT_PROFILE_ID.to_owned(),
        name: "Default".to_owned(),
    })
}

/// What a new profile copies from the active one. All `false`: a fresh profile, as if GitYas had just been installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CarryOver {
    /// Registered repositories, with the open tabs.
    pub repositories: bool,
    /// Appearance and behaviour settings (theme, diff view, graph options, layout, background fetch).
    pub settings: bool,
    /// The commit signature (name and email).
    pub signature: bool,
    /// Git hosting credentials (personal access tokens) in the keychain.
    pub credentials: bool,
}

impl CarryOver {
    pub const EVERYTHING: CarryOver = CarryOver {
        repositories: true,
        settings: true,
        signature: true,
        credentials: true,
    };
}

/// The registry of profiles (`profiles.json`) and where each profile keeps its data. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    base: Paths,
    service: String,
}

impl ProfileStore {
    /// Profiles under the shipped directories and keychain service.
    pub fn new(base: Paths) -> Self {
        Self::with_service(base, KEYCHAIN_SERVICE)
    }

    /// Profiles under a different keychain service (tests).
    pub fn with_service(base: Paths, service: impl Into<String>) -> Self {
        Self {
            base,
            service: service.into(),
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.base.data_dir.join(REGISTRY_FILE)
    }

    /// The registry. A missing file means only the Default profile; an unreadable one is logged and treated the
    /// same (the profiles' data stays on disk, so fixing the file brings them back).
    pub fn load(&self) -> Profiles {
        let path = self.registry_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Profiles>(&text) {
                Ok(profiles) => profiles.normalized(),
                Err(err) => {
                    tracing::error!(path = %path.display(), %err, "cannot parse the profiles file; using Default");
                    Profiles::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Profiles::default(),
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "cannot read the profiles file; using Default");
                Profiles::default()
            }
        }
    }

    /// Writes the registry atomically (temp file + rename).
    fn save(&self, profiles: &Profiles) -> Result<()> {
        let path = self.registry_path();
        let text =
            serde_json::to_string_pretty(profiles).map_err(|err| GitYasError::Other(format!("cannot encode the profiles: {err}")))?;
        std::fs::create_dir_all(&self.base.data_dir)
            .map_err(|err| GitYasError::Io(format!("cannot create {}: {err}", self.base.data_dir.display())))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|err| GitYasError::Io(format!("cannot write {}: {err}", tmp.display())))?;
        std::fs::rename(&tmp, &path).map_err(|err| GitYasError::Io(format!("cannot write {}: {err}", path.display())))
    }

    /// Data and cache directories of profile `id` (the log directory is shared).
    pub fn paths(&self, id: &str) -> Paths {
        if id == DEFAULT_PROFILE_ID {
            return self.base.clone();
        }
        Paths {
            data_dir: self.base.data_dir.join(PROFILES_DIR).join(id),
            cache_dir: self.base.cache_dir.join(PROFILES_DIR).join(id),
            log_dir: self.base.log_dir.clone(),
        }
    }

    /// Keychain service of profile `id`.
    pub fn keychain_service(&self, id: &str) -> String {
        if id == DEFAULT_PROFILE_ID {
            self.service.clone()
        } else {
            format!("{}.profile.{id}", self.service)
        }
    }

    /// Adds a profile named `name` (trimmed; `InvalidInput` if blank, too long or taken) and creates its data
    /// directory. Does not make it active and copies nothing: see [`Engine::copy_to_profile`].
    pub fn add(&self, name: &str) -> Result<Profile> {
        let mut profiles = self.load();
        let name = unique_name(&profiles, None, name)?;
        let id = loop {
            let id = nanoid::nanoid!(10, &nanoid::alphabet::SAFE[2..]);
            if valid_id(&id) && profiles.get(&id).is_none() {
                break id;
            }
        };
        let profile = Profile { id, name };
        self.paths(&profile.id).create_dirs()?;
        profiles.profiles.push(profile.clone());
        self.save(&profiles)?;
        Ok(profile)
    }

    /// Renames profile `id`.
    pub fn rename(&self, id: &str, name: &str) -> Result<Profiles> {
        let mut profiles = self.load();
        let name = unique_name(&profiles, Some(id), name)?;
        let profile = profiles
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| GitYasError::NotFound(format!("profile {id}")))?;
        profile.name = name;
        self.save(&profiles)?;
        Ok(profiles)
    }

    /// Makes `id` the profile the next launch (and the next engine) uses.
    pub fn set_active(&self, id: &str) -> Result<Profiles> {
        let mut profiles = self.load();
        if profiles.get(id).is_none() {
            return Err(GitYasError::NotFound(format!("profile {id}")));
        }
        profiles.active = id.to_owned();
        self.save(&profiles)?;
        Ok(profiles)
    }

    /// Deletes profile `id`: its registry entry, its directories and its keychain items. The Default profile and
    /// the active profile cannot be deleted (`InvalidInput`).
    pub fn remove(&self, id: &str) -> Result<Profiles> {
        let mut profiles = self.load();
        if id == DEFAULT_PROFILE_ID {
            return Err(GitYasError::InvalidInput("the Default profile cannot be deleted".into()));
        }
        if profiles.active == id {
            return Err(GitYasError::InvalidInput(
                "switch to another profile before deleting this one".into(),
            ));
        }
        if profiles.get(id).is_none() {
            return Err(GitYasError::NotFound(format!("profile {id}")));
        }
        profiles.profiles.retain(|p| p.id != id);
        self.save(&profiles)?;
        let paths = self.paths(id);
        for dir in [&paths.data_dir, &paths.cache_dir] {
            remove_dir(dir);
        }
        let keychain = KeyringStore::with_service(self.keychain_service(id));
        for provider in Provider::ALL {
            if let Err(err) = keychain.delete(provider) {
                tracing::warn!(%err, "cannot remove a deleted profile's {} credential", provider.key());
            }
        }
        Ok(profiles)
    }
}

fn remove_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(dir = %dir.display(), %err, "cannot remove a deleted profile's directory"),
    }
}

/// Ids name directories and keychain services: ASCII letters, digits, `-` and `_` only.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Trims `name` and checks it is usable as a profile name.
pub fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(GitYasError::InvalidInput("Give the profile a name".into()));
    }
    if name.chars().count() > MAX_PROFILE_NAME {
        return Err(GitYasError::InvalidInput(format!(
            "Profile names are at most {MAX_PROFILE_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(GitYasError::InvalidInput("Profile names cannot contain line breaks".into()));
    }
    Ok(name.to_owned())
}

/// [`clean_name`], and no other profile than `except` already uses it (case-insensitively).
fn unique_name(profiles: &Profiles, except: Option<&str>, name: &str) -> Result<String> {
    let name = clean_name(name)?;
    let taken = profiles
        .profiles
        .iter()
        .any(|p| Some(p.id.as_str()) != except && p.name.to_lowercase() == name.to_lowercase());
    if taken {
        return Err(GitYasError::AlreadyExists(format!("A profile named {name} already exists")));
    }
    Ok(name)
}

impl Engine {
    /// [`Engine::open`] for profile `id` of `store`: its own databases and keychain service.
    pub fn open_profile(
        store: &ProfileStore,
        id: &str,
        events: Arc<dyn gityas_core::EventSink>,
        runtime: tokio::runtime::Handle,
    ) -> Result<Engine> {
        let paths = store.paths(id);
        paths.create_dirs()?;
        let storage_paths = StoragePaths {
            data_dir: paths.data_dir.clone(),
            cache_dir: paths.cache_dir.clone(),
        };
        let (storage, report) = Storage::open(&storage_paths)?;
        let credentials = Arc::new(KeyringStore::with_service(store.keychain_service(id)));
        let engine = Engine::new(storage, credentials, events, runtime);
        engine.report_storage_warnings(report.warnings);
        Ok(engine)
    }

    /// Copies what `carry` selects from this engine's profile into profile `id` of `store` (a new, empty profile).
    /// Credentials that cannot be read (keychain locked) are skipped and logged rather than failing the copy.
    pub async fn copy_to_profile(&self, store: &ProfileStore, id: &str, carry: CarryOver) -> Result<()> {
        if carry == CarryOver::default() {
            return Ok(());
        }
        let inner = Arc::clone(&self.inner);
        let paths = store.paths(id);
        let service = store.keychain_service(id);
        self.inner
            .blocking(move || {
                let (target, _) = Storage::open(&StoragePaths {
                    data_dir: paths.data_dir.clone(),
                    cache_dir: paths.cache_dir.clone(),
                })?;
                let source = inner.storage.user();
                let into = target.user();
                if carry.repositories {
                    let mut ids: Vec<(RepoId, RepoId)> = Vec::new();
                    for repo in source.repositories()? {
                        match into.insert_repository(&repo.name, &repo.path, repo.provider) {
                            Ok(copy) => ids.push((repo.id, copy.id)),
                            Err(err) => tracing::warn!(%err, "cannot copy repository {}", repo.name),
                        }
                    }
                    let map = |id: &RepoId| ids.iter().find(|(old, _)| old == id).map(|(_, new)| new.clone());
                    let tabs = source.open_tabs()?;
                    let copied = OpenTabs {
                        open: tabs.open.iter().filter_map(map).collect(),
                        active: tabs.active.as_ref().and_then(map),
                    };
                    into.set_open_tabs(&copied)?;
                }
                if carry.settings || carry.signature {
                    let from = source.settings()?;
                    let mut settings = into.settings()?;
                    if carry.settings {
                        let signature = settings.commit_signature.take();
                        settings = from.clone();
                        settings.commit_signature = signature;
                    }
                    if carry.signature {
                        settings.commit_signature = from.commit_signature;
                    }
                    into.save_settings(&settings)?;
                }
                if carry.credentials {
                    let keychain = KeyringStore::with_service(service);
                    for provider in Provider::ALL {
                        match inner.credentials.try_credential(provider) {
                            Ok(Some(credential)) => {
                                if let Err(err) = keychain.set(provider, credential) {
                                    tracing::warn!(%err, "cannot copy the {} credential", provider.key());
                                }
                            }
                            Ok(None) => {}
                            Err(err) => tracing::warn!(%err, "cannot read the {} credential to copy", provider.key()),
                        }
                    }
                }
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let paths = Paths::under(dir.path());
        paths.create_dirs().unwrap_or_else(|e| panic!("{e}"));
        (dir, ProfileStore::with_service(paths, "com.gityas.GitYas.test-profiles"))
    }

    #[test]
    fn starts_with_the_default_profile() {
        let (_dir, store) = store();
        let profiles = store.load();
        assert_eq!(profiles, Profiles::default());
        assert_eq!(store.paths(DEFAULT_PROFILE_ID), store.base);
    }

    #[test]
    fn add_rename_switch_and_remove() -> Result<()> {
        let (_dir, store) = store();
        let work = store.add("  Work ")?;
        assert_eq!(work.name, "Work");
        assert!(store.paths(&work.id).data_dir.is_dir());
        assert!(matches!(store.add("work"), Err(GitYasError::AlreadyExists(_))));
        assert!(matches!(store.add("  "), Err(GitYasError::InvalidInput(_))));

        let profiles = store.rename(&work.id, "Day job")?;
        assert_eq!(profiles.get(&work.id).map(|p| p.name.as_str()), Some("Day job"));
        store.rename(DEFAULT_PROFILE_ID, "Personal")?;

        store.set_active(&work.id)?;
        assert_eq!(store.load().active, work.id);
        assert!(store.remove(&work.id).is_err(), "the active profile stays");
        assert!(store.remove(DEFAULT_PROFILE_ID).is_err());

        store.set_active(DEFAULT_PROFILE_ID)?;
        let profiles = store.remove(&work.id)?;
        assert_eq!(profiles.profiles.len(), 1);
        assert_eq!(profiles.profiles[0].name, "Personal");
        assert!(!store.paths(&work.id).data_dir.exists());
        Ok(())
    }

    #[test]
    fn a_damaged_registry_falls_back_and_is_repaired() -> Result<()> {
        let (_dir, store) = store();
        std::fs::write(store.registry_path(), "{nope").map_err(|e| GitYasError::Io(e.to_string()))?;
        assert_eq!(store.load(), Profiles::default());
        let edited = r#"{"active":"gone","profiles":[{"id":"a b","name":"x"},{"id":"w1","name":""}]}"#;
        std::fs::write(store.registry_path(), edited).map_err(|e| GitYasError::Io(e.to_string()))?;
        let profiles = store.load();
        assert_eq!(profiles.active, DEFAULT_PROFILE_ID);
        let ids: Vec<_> = profiles.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, [DEFAULT_PROFILE_ID, "w1"]);
        assert_eq!(profiles.profiles[1].name, "Profile");
        Ok(())
    }
}
