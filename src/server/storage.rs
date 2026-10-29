//! Versioned workspace metadata. Live processes and terminal output are not stored.
use crate::core::Workspace;
use anyhow::{Context, Result, bail, ensure};
use nix::unistd::geteuid;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_PANES: usize = 16;
const MAX_LABEL_CHARACTERS: usize = 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataDocument {
    format_version: u32,
    workspace: Workspace,
}

/// An exclusively locked metadata destination for one server incarnation.
pub struct MetadataStore {
    path: PathBuf,
    directory: File,
    // The lifetime of this handle is the lifetime of the exclusive lock.
    _lock: File,
}

impl MetadataStore {
    /// Open a private metadata destination, creating its directory when needed.
    ///
    /// # Errors
    /// Rejects relative paths, unsafe existing entries or a lock held by another
    /// process. Existing metadata is never truncated by opening the store.
    pub fn open(path: PathBuf) -> Result<Self> {
        ensure!(path.is_absolute(), "metadata path must be absolute");
        let parent = path.parent().context("metadata path needs a parent")?;
        let name = path
            .file_name()
            .context("metadata path needs a file name")?;
        make_private_directory(parent)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY)
            .open(parent)
            .context("open metadata directory")?;
        check_directory(&directory.metadata()?)?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path.with_file_name(lock_name))
            .context("open metadata lock")?;
        check_private_file(&lock.metadata()?, "metadata lock")?;
        lock.try_lock()
            .context("metadata is owned by another server")?;
        inspect_existing(&path)?;
        Ok(Self {
            path,
            directory,
            _lock: lock,
        })
    }

    /// Load metadata, leaving absent state distinct from invalid state.
    ///
    /// # Errors
    /// Rejects corrupt, oversized, unsafe, unsupported-version or invalid state.
    /// Callers must report these errors rather than replacing the saved layout.
    pub fn load(&self) -> Result<Option<Workspace>> {
        if inspect_existing(&self.path)?.is_none() {
            return Ok(None);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&self.path)
            .context("open workspace metadata")?;
        let metadata = file.metadata()?;
        check_private_file(&metadata, "workspace metadata")?;
        ensure!(
            metadata.len() <= MAX_METADATA_BYTES as u64,
            "workspace metadata exceeds 1 MiB"
        );
        let mut bytes = Vec::new();
        file.take((MAX_METADATA_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("read workspace metadata")?;
        ensure!(
            bytes.len() <= MAX_METADATA_BYTES,
            "workspace metadata exceeds 1 MiB"
        );
        let document: MetadataDocument = serde_json::from_slice(&bytes)
            .context("decode workspace metadata; saved state was not changed")?;
        ensure!(
            document.format_version == FORMAT_VERSION,
            "unsupported workspace metadata version {}",
            document.format_version
        );
        validate_workspace(&document.workspace)?;
        Ok(Some(document.workspace))
    }

    /// Atomically replace metadata only after validating and syncing new bytes.
    ///
    /// # Errors
    /// Returns an error on invalid state, unsafe destinations or filesystem
    /// failure. Failures before rename preserve the previous metadata. A failed
    /// directory sync after rename reports uncertain durability of valid new data.
    pub fn save(&self, workspace: &Workspace) -> Result<()> {
        validate_workspace(workspace)?;
        let mut bytes = BoundedMetadata(Vec::new());
        serde_json::to_writer(
            &mut bytes,
            &MetadataDocument {
                format_version: FORMAT_VERSION,
                workspace: workspace.clone(),
            },
        )
        .context("encode workspace metadata")?;
        inspect_existing(&self.path)?;
        let (mut file, mut temporary) = create_temporary(&self.path)?;
        file.write_all(&bytes.0)
            .context("write temporary workspace metadata")?;
        file.sync_all()
            .context("sync temporary workspace metadata")?;
        // Check again immediately before replacement, including a missing path
        // that may have been replaced with a non-file after temporary creation.
        inspect_existing(&self.path)?;
        fs::rename(temporary.path(), &self.path).context("replace workspace metadata")?;
        temporary.disarm();
        self.directory
            .sync_all()
            .context("sync metadata directory after replacement")
    }
}

/// Resolve the durable state location without modifying the filesystem.
///
/// # Errors
/// Returns an error for relative XDG/HOME configuration or missing HOME.
pub fn default_state_path() -> Result<PathBuf> {
    let parent = if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        let path = PathBuf::from(path);
        ensure!(path.is_absolute(), "XDG_STATE_HOME must be absolute");
        path
    } else {
        let home = PathBuf::from(
            std::env::var_os("HOME").context("HOME is required to resolve workspace metadata")?,
        );
        ensure!(home.is_absolute(), "HOME must be absolute");
        home.join(".local/state")
    };
    Ok(parent.join("rex-rs/workspace.json"))
}

fn validate_workspace(workspace: &Workspace) -> Result<()> {
    ensure!(
        workspace.is_valid(),
        "workspace metadata violates state invariants"
    );
    ensure!(
        workspace.pane_ids().len() <= MAX_PANES,
        "workspace metadata exceeds pane limit"
    );
    for session in workspace.sessions() {
        ensure!(
            session.title().chars().count() <= MAX_LABEL_CHARACTERS,
            "saved session label exceeds limit"
        );
        for window in session.windows() {
            ensure!(
                window.title().chars().count() <= MAX_LABEL_CHARACTERS,
                "saved tab label exceeds limit"
            );
        }
    }
    Ok(())
}

fn make_private_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().recursive(true).mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("create metadata directory"),
    }
    let metadata = fs::symlink_metadata(path).context("inspect metadata directory")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "metadata directory must not be a symlink"
    );
    check_directory(&metadata)
}

fn check_directory(metadata: &Metadata) -> Result<()> {
    ensure!(
        metadata.is_dir()
            && metadata.uid() == geteuid().as_raw()
            && metadata.mode() & 0o7777 == 0o700,
        "metadata directory must be mode 0700 and owned by this user"
    );
    Ok(())
}

fn check_private_file(metadata: &Metadata, description: &str) -> Result<()> {
    ensure!(
        metadata.is_file()
            && metadata.uid() == geteuid().as_raw()
            && metadata.mode() & 0o7777 == 0o600,
        "{description} must be a mode 0600 regular file owned by this user"
    );
    Ok(())
}

fn inspect_existing(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            check_private_file(&metadata, "workspace metadata")?;
            ensure!(
                metadata.len() <= MAX_METADATA_BYTES as u64,
                "workspace metadata exceeds 1 MiB"
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("inspect workspace metadata"),
    }
}

struct BoundedMetadata(Vec<u8>);
impl Write for BoundedMetadata {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_METADATA_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "workspace metadata exceeds 1 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TemporaryMetadata {
    path: PathBuf,
    armed: bool,
}
impl TemporaryMetadata {
    fn path(&self) -> &Path {
        &self.path
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for TemporaryMetadata {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_temporary(path: &Path) -> Result<(File, TemporaryMetadata)> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for _ in 0..8 {
        let mut name = OsString::from(".");
        name.push(
            path.file_name()
                .context("metadata path needs a file name")?,
        );
        name.push(format!(
            ".tmp-{}-{time}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&temporary)
        {
            Ok(file) => {
                return Ok((
                    file,
                    TemporaryMetadata {
                        path: temporary,
                        armed: true,
                    },
                ));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create temporary workspace metadata"),
        }
    }
    bail!("could not allocate a unique workspace metadata temporary file")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests fail immediately when metadata fixture construction fails."
)]
mod tests {
    use super::*;
    use crate::core::{Command, SplitAxis};
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rex-metadata-test-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("workspace.json")
        }
        fn write(&self, bytes: &[u8]) {
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(self.path())
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn workspace() -> Workspace {
        let mut state = Workspace::default();
        state
            .apply(Command::CreateSession {
                title: "Saved 界".into(),
            })
            .unwrap();
        let pane = state.pane_ids()[0];
        state
            .apply(Command::SplitPane {
                pane,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let split = state.sessions()[0].layout().unwrap().split_ids()[0];
        state
            .apply(Command::ResizeSplit { split, ratio: 320 })
            .unwrap();
        state.apply(Command::ToggleZoom(pane)).unwrap();
        let session = state.sessions()[0].id();
        state
            .apply(Command::CreateWindow {
                session,
                title: "Second tab".into(),
            })
            .unwrap();
        state
    }

    #[test]
    fn round_trip_preserves_layout_labels_focus_and_allocator() {
        let fixture = Fixture::new();
        let store = MetadataStore::open(fixture.path()).unwrap();
        assert_eq!(store.load().unwrap(), None);
        let original = workspace();
        store.save(&original).unwrap();
        let mut restored = store.load().unwrap().unwrap();
        assert_eq!(restored, original);
        let mut expected = original;
        let next = Command::CreateSession {
            title: "After recovery".into(),
        };
        restored.apply(next.clone()).unwrap();
        expected.apply(next).unwrap();
        assert_eq!(restored, expected);
        assert_eq!(fs::metadata(fixture.path()).unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn unsupported_corrupt_and_invalid_state_are_not_replaced() {
        let fixture = Fixture::new();
        let store = MetadataStore::open(fixture.path()).unwrap();
        for bytes in [b"not json".as_slice(), br#"{"format_version":99,"workspace":{"sessions":[],"active_session":null,"next_id":0}}"#] {
            fixture.write(bytes);
            assert!(store.load().is_err());
            assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
        }
        let mut value = serde_json::to_value(MetadataDocument {
            format_version: 1,
            workspace: workspace(),
        })
        .unwrap();
        value["workspace"]["sessions"][0]["active_window"] = serde_json::json!(99);
        let bytes = serde_json::to_vec(&value).unwrap();
        fixture.write(&bytes);
        assert!(store.load().is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }

    #[test]
    fn oversize_and_invalid_save_preserve_previous_file() {
        let fixture = Fixture::new();
        let store = MetadataStore::open(fixture.path()).unwrap();
        let original = workspace();
        store.save(&original).unwrap();
        let bytes = fs::read(fixture.path()).unwrap();
        let mut invalid = original;
        let id = invalid.sessions()[0].id();
        invalid
            .apply(Command::RenameSession {
                session: id,
                title: "x".repeat(129),
            })
            .unwrap();
        assert!(store.save(&invalid).is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
        fixture.write(&vec![b' '; MAX_METADATA_BYTES + 1]);
        assert!(store.load().is_err());
        assert_eq!(
            fs::metadata(fixture.path()).unwrap().len(),
            (MAX_METADATA_BYTES + 1) as u64
        );
    }

    #[test]
    fn exclusive_lock_and_atomic_replacement_leave_no_temporaries() {
        let fixture = Fixture::new();
        let store = MetadataStore::open(fixture.path()).unwrap();
        assert!(MetadataStore::open(fixture.path()).is_err());
        store.save(&workspace()).unwrap();
        let first_inode = fs::metadata(fixture.path()).unwrap().ino();
        store.save(&Workspace::default()).unwrap();
        assert_ne!(fs::metadata(fixture.path()).unwrap().ino(), first_inode);
        assert_eq!(store.load().unwrap(), Some(Workspace::default()));
        let names: Vec<_> = fs::read_dir(&fixture.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2);
        drop(store);
        assert!(MetadataStore::open(fixture.path()).is_ok());
    }

    #[test]
    fn unsafe_destinations_and_nonprivate_directories_are_preserved() {
        let fixture = Fixture::new();
        let target = fixture.0.join("keep-me");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, fixture.path()).unwrap();
        assert!(MetadataStore::open(fixture.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        fs::remove_file(fixture.path()).unwrap();
        fs::create_dir(fixture.path()).unwrap();
        assert!(MetadataStore::open(fixture.path()).is_err());
        fs::remove_dir(fixture.path()).unwrap();
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(MetadataStore::open(fixture.path()).is_err());
    }

    #[test]
    fn save_refuses_an_unsafe_destination_created_after_open() {
        let fixture = Fixture::new();
        let store = MetadataStore::open(fixture.path()).unwrap();
        let target = fixture.0.join("keep-me");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, fixture.path()).unwrap();
        assert!(store.save(&workspace()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        assert!(
            fs::symlink_metadata(fixture.path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
