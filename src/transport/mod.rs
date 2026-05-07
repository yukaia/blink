//! Connection abstraction.
//!
//! Adding a new protocol means:
//!   1. Implement [`Transport`] in a new file under `transport/`.
//!   2. Add a match arm in [`open`].
//!
//! Everything else in the app (TUI, transfer manager, session model) talks to
//! `Box<dyn Transport>`, not to a specific protocol.

use std::path::Path;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::session::{Protocol, Session};

pub mod ftp;
pub(crate) mod ftp_impl;
pub mod ftps;
pub mod scp;
pub mod sftp;

/// One entry from a remote directory listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Populated by SFTP/SCP; `None` for FTP (protocol doesn't report it in LIST).
    /// Not yet rendered in the file pane — reserved for a future column.
    #[allow(dead_code)]
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    /// POSIX mode bits; `None` for FTP. Reserved for a future permissions column.
    #[allow(dead_code)]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl RemoteEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Directory)
    }
}

/// Progress update emitted while a single file is in flight.
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Maximum time allowed for `transport::open` (TCP connect + SSH handshake +
/// auth). Shared between the TUI initial-connect path and the dispatcher's
/// per-job connect path so both enforce the same deadline.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What every protocol implementation must provide.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Human-readable label, e.g. `Protocol::Sftp`.
    #[allow(dead_code)]
    fn protocol(&self) -> Protocol;

    /// List entries in `remote_path`. Implementations must NOT include `.` or `..`.
    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>>;

    /// Download `remote_path` to `local_path`, sending progress to `progress`
    /// if a sender is provided.
    async fn download(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()>;

    /// Upload `local_path` to `remote_path`.
    async fn upload(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()>;

    /// Rename / move on the remote side.
    async fn rename(&mut self, from: &str, to: &str) -> Result<()>;

    /// Delete a single remote file.
    async fn delete_file(&mut self, remote_path: &str) -> Result<()>;

    /// Delete a remote directory.
    ///
    /// When `recursive` is `false`, the implementation issues a single
    /// `rmdir`-equivalent call; the operation fails on non-empty directories.
    /// When `recursive` is `true`, the implementation walks `remote_path`
    /// post-order and removes every descendant before removing the root.
    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()>;

    /// Create a remote directory. Implementations should treat "already
    /// exists" as a non-error since recursive uploads call this best-effort
    /// for every level of the tree.
    async fn mkdir(&mut self, remote_path: &str) -> Result<()>;

    /// Stat a single remote path. Returns `None` if the path doesn't exist.
    /// Used by recursive walks and overwrite checks.
    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>>;

    /// Read a remote file fully into memory. Used for previewing small text
    /// files and images.
    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes>;

    /// Cleanly close the connection.
    async fn close(&mut self) -> Result<()>;
}

/// Build the right transport for `session`. The password (if any) must be
/// resolved by the caller before this is invoked — we never store it on disk.
///
/// `app_event_tx` is forwarded to the SFTP/SCP handler for the host-key
/// confirmation flow. FTP/FTPS do not use host-key verification.
pub async fn open(
    session: &Session,
    password: Option<&str>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
) -> Result<Box<dyn Transport>> {
    match session.protocol {
        Protocol::Sftp => Ok(Box::new(
            sftp::SftpTransport::connect(session, password, app_event_tx).await?,
        )),
        Protocol::Scp => Ok(Box::new(
            scp::ScpTransport::connect(session, password, app_event_tx).await?,
        )),
        Protocol::Ftp => Ok(Box::new(ftp::FtpTransport::connect(session, password).await?)),
        Protocol::Ftps => Ok(Box::new(ftps::FtpsTransport::connect(session, password).await?)),
    }
}

/// Join a remote base path and a name, normalising the slash.
///
/// `name` must be a single path component (a filename from a directory
/// listing). Leading slashes are stripped to prevent a server-controlled name
/// like `"/etc/shadow"` from producing an absolute remote path via the `//`
/// resolution most servers apply. Names containing a `..` component are
/// rejected (returning `base` unchanged) to prevent upward traversal.
pub(crate) fn join_remote(base: &str, name: &str) -> String {
    let name = name.trim_start_matches('/');
    // Reject `.` and `..` components: `..` traverses upward; `.` is a no-op
    // but would produce paths like `/foo/./bar` that some servers don't
    // normalise, and a server-controlled `.` in a name is almost always
    // malicious.
    if name.split('/').any(|c| c == ".." || c == ".") {
        return base.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Compute the parent of a remote path.
pub(crate) fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => "/".to_string(),
    }
}

/// In-memory mock transport for testing transfer logic without a real server.
///
/// Stores files in a `HashMap<String, Vec<u8>>` keyed by remote path.
/// Directory structure is implicit — any path can be listed if it was created
/// via `mkdir`, and any path can hold a file via `upload`.
#[cfg(test)]
pub(crate) mod mock {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::mpsc;

    use crate::error::Result;
    use crate::session::Protocol;
    use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

    #[derive(Debug, Clone)]
    pub struct MockTransport {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        dirs: Arc<Mutex<Vec<String>>>,
    }

    impl MockTransport {
        #[allow(dead_code)]
        pub fn new() -> Self {
            Self {
                files: Arc::new(Mutex::new(HashMap::new())),
                dirs: Arc::new(Mutex::new(vec!["/".to_string()])),
            }
        }

        #[allow(dead_code)]
        pub fn with_file(self, path: &str, contents: &[u8]) -> Self {
            let mut parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("/");
            if parent.is_empty() {
                parent = "/";
            }
            self.dirs.lock().unwrap().push(parent.to_string());
            self.files.lock().unwrap().insert(path.to_string(), contents.to_vec());
            self
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        fn protocol(&self) -> Protocol {
            Protocol::Sftp
        }

        async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
            let p = if remote_path.ends_with('/') {
                remote_path.to_string()
            } else {
                format!("{remote_path}/")
            };
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();

            if !dirs.contains(&remote_path.to_string()) && remote_path != "/" {
                return Err(crate::error::BlinkError::transport(format!(
                    "no such directory: {remote_path}"
                )));
            }

            let mut entries: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for path in files.keys() {
                if let Some(rest) = path.strip_prefix(&p) {
                    if let Some(name) = rest.split('/').next() {
                        if !name.is_empty() {
                            entries.insert(name.to_string());
                        }
                    }
                }
            }
            for dir in dirs.iter() {
                if let Some(rest) = dir.strip_prefix(&p) {
                    if let Some(name) = rest.split('/').next() {
                        if !name.is_empty() {
                            entries.insert(name.to_string());
                        }
                    }
                }
            }

            let mut out = Vec::new();
            for name in entries {
                let is_dir = {
                    let full = format!("{}{}", p, name);
                    dirs.contains(&full)
                };
                let size = if is_dir {
                    0
                } else {
                    let full = format!("{}{}", p, name);
                    files.get(&full).map(|b| b.len() as u64).unwrap_or(0)
                };
                out.push(RemoteEntry {
                    name,
                    kind: if is_dir {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    },
                    size,
                    modified: None,
                    mode: None,
                });
            }
            Ok(out)
        }

        async fn download(
            &mut self,
            remote_path: &str,
            local_path: &Path,
            progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> Result<()> {
            let data = {
                let files = self.files.lock().unwrap();
                files
                    .get(remote_path)
                    .cloned()
                    .ok_or_else(|| {
                        crate::error::BlinkError::transport(format!(
                            "file not found: {remote_path}"
                        ))
                    })?
            };
            if let Some(parent) = local_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(local_path, &data).await?;
            if let Some(tx) = &progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: data.len() as u64,
                    bytes_total: data.len() as u64,
                });
            }
            Ok(())
        }

        async fn upload(
            &mut self,
            local_path: &Path,
            remote_path: &str,
            _progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> Result<()> {
            let data = tokio::fs::read(local_path).await?;
            self.files
                .lock()
                .unwrap()
                .insert(remote_path.to_string(), data);
            Ok(())
        }

        async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
            let mut files = self.files.lock().unwrap();
            if let Some(data) = files.remove(from) {
                files.insert(to.to_string(), data);
                Ok(())
            } else {
                Err(crate::error::BlinkError::transport(format!(
                    "file not found: {from}"
                )))
            }
        }

        async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
            self.files.lock().unwrap().remove(remote_path);
            Ok(())
        }

        async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
            let mut dirs = self.dirs.lock().unwrap();
            if recursive {
                dirs.retain(|d| !d.starts_with(remote_path));
                self.files.lock().unwrap().retain(|k, _| {
                    !k.starts_with(remote_path)
                });
            } else {
                dirs.retain(|d| d != remote_path);
            }
            Ok(())
        }

        async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
            self.dirs
                .lock()
                .unwrap()
                .push(remote_path.to_string());
            Ok(())
        }

        async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();
            if let Some(data) = files.get(remote_path) {
                let name = remote_path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(remote_path)
                    .to_string();
                return Ok(Some(RemoteEntry {
                    name,
                    kind: EntryKind::File,
                    size: data.len() as u64,
                    modified: None,
                    mode: None,
                }));
            }
            if dirs.contains(&remote_path.to_string()) {
                let name = remote_path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(remote_path)
                    .to_string();
                return Ok(Some(RemoteEntry {
                    name,
                    kind: EntryKind::Directory,
                    size: 0,
                    modified: None,
                    mode: None,
                }));
            }
            Ok(None)
        }

        async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
            let files = self.files.lock().unwrap();
            files
                .get(remote_path)
                .cloned()
                .map(Bytes::from)
                .ok_or_else(|| {
                    crate::error::BlinkError::transport(format!(
                        "file not found: {remote_path}"
                    ))
                })
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // join_remote
    #[test]
    fn join_appends_name() {
        assert_eq!(join_remote("/home/user", "file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn join_trailing_slash_base() {
        assert_eq!(join_remote("/home/user/", "file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn join_strips_leading_slash_from_name() {
        assert_eq!(join_remote("/srv", "/etc/shadow"), "/srv/etc/shadow");
    }

    #[test]
    fn join_rejects_dotdot_traversal() {
        assert_eq!(join_remote("/srv/data", "../secret"), "/srv/data");
    }

    #[test]
    fn join_rejects_embedded_dotdot() {
        assert_eq!(join_remote("/srv/data", "a/../b"), "/srv/data");
    }

    #[test]
    fn join_rejects_single_dot() {
        assert_eq!(join_remote("/srv/data", "."), "/srv/data");
    }

    #[test]
    fn join_rejects_embedded_single_dot() {
        assert_eq!(join_remote("/srv/data", "a/./b"), "/srv/data");
    }

    #[test]
    fn join_root_base() {
        assert_eq!(join_remote("/", "etc"), "/etc");
    }

    // parent_remote
    #[test]
    fn parent_of_root_is_root() {
        assert_eq!(parent_remote("/"), "/");
    }

    #[test]
    fn parent_of_file_in_root() {
        assert_eq!(parent_remote("/file.txt"), "/");
    }

    #[test]
    fn parent_of_nested_path() {
        assert_eq!(parent_remote("/home/user/docs"), "/home/user");
    }

    #[test]
    fn parent_strips_trailing_slash() {
        assert_eq!(parent_remote("/home/user/docs/"), "/home/user");
    }

    #[test]
    fn parent_of_empty_is_root() {
        assert_eq!(parent_remote(""), "/");
    }

    // -----------------------------------------------------------------------
    // MockTransport tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_list_empty() {
        let mut m = mock::MockTransport::new();
        let entries = m.list("/").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn mock_list_with_file() {
        let mut m = mock::MockTransport::new().with_file("/hello.txt", b"world");
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");
        assert!(!entries[0].is_dir());
        assert_eq!(entries[0].size, 5);
    }

    #[tokio::test]
    async fn mock_list_with_dir() {
        let mut m = mock::MockTransport::new();
        m.mkdir("/subdir").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "subdir");
    }

    #[tokio::test]
    async fn mock_upload_and_download() {
        let dir = std::env::temp_dir().join("blink-mock-test");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let local = dir.join(format!("upload-{}", std::process::id()));

        let mut m = mock::MockTransport::new();
        tokio::fs::write(&local, b"hello from mock").await.unwrap();
        m.upload(&local, "/remote.txt", None).await.unwrap();

        let dest = dir.join("downloaded.txt");
        m.download("/remote.txt", &dest, None).await.unwrap();
        let data = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(data, b"hello from mock");

        let _ = tokio::fs::remove_file(&local).await;
        let _ = tokio::fs::remove_file(&dest).await;
    }

    #[tokio::test]
    async fn mock_rename() {
        let mut m = mock::MockTransport::new().with_file("/old.txt", b"data");
        m.rename("/old.txt", "/new.txt").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "new.txt");
        assert!(m.metadata("/old.txt").await.unwrap().is_none());
        assert!(m.metadata("/new.txt").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn mock_delete() {
        let mut m = mock::MockTransport::new()
            .with_file("/a.txt", b"aaa")
            .with_file("/b.txt", b"bbb");
        m.delete_file("/a.txt").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b.txt");
    }

    #[tokio::test]
    async fn mock_delete_dir_recursive() {
        let mut m = mock::MockTransport::new();
        m.mkdir("/dir").await.unwrap();
        let mut inner = mock::MockTransport::new();
        inner.mkdir("/dir/sub").await.unwrap();
        // Add a file inside the subdirectory via the shared transport
        m = inner;
        m.delete_dir("/dir", true).await.unwrap();
        assert!(m.metadata("/dir").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_read_to_bytes() {
        let mut m = mock::MockTransport::new().with_file("/data.bin", b"\x00\x01\x02");
        let bytes = m.read_to_bytes("/data.bin").await.unwrap();
        assert_eq!(&bytes[..], &[0, 1, 2]);
    }

    #[tokio::test]
    async fn mock_metadata_file() {
        let mut m = mock::MockTransport::new().with_file("/f", b"12345");
        let meta = m.metadata("/f").await.unwrap().unwrap();
        assert_eq!(meta.name, "f");
        assert!(meta.is_dir() == false);
        assert_eq!(meta.size, 5);
    }

    #[tokio::test]
    async fn mock_metadata_not_found() {
        let mut m = mock::MockTransport::new();
        assert!(m.metadata("/nope").await.unwrap().is_none());
    }
}

