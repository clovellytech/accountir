//! Files attached to a set of books: the statements a taxpayer receives (W-2s,
//! 1099s, K-1s from partnerships nobody here keeps books for), a notice a
//! business was sent, anything a return has to be able to point at.
//!
//! # Why the file is not in the event log
//!
//! The log is replicated in full to every member's machine, hashed, and kept
//! forever. A 2 MB scan in it is 2 MB in every replica and every export, and a
//! statement carries the recipient's SSN — which `partnership_commands` already
//! decided must never enter the log. So the log records *that* a document was
//! attached and the SHA-256 of its bytes; the bytes live in a [`BlobStore`],
//! addressed by that digest.
//!
//! Addressing by content is what makes the bytes safe to move later:
//!
//! - **Verifiable.** Whoever holds a copy can prove it is the one the log names,
//!   so bytes fetched from a server, a backup or another laptop are checked
//!   rather than trusted. [`BlobStore::get`] refuses a mismatch.
//! - **Idempotent.** Storing the same bytes twice is one blob, so a retried
//!   transfer cannot duplicate or half-replace anything.
//! - **Separable.** A replica can hold the log without the files — a phone that
//!   fetches a statement only when somebody opens it — and knows exactly which
//!   files it is missing.
//!
//! # Where the bytes live
//!
//! Locally, under `~/.local/share/accountir/documents/<ledger id>/`, one folder
//! per set of books, keyed by the ledger's `company_id` rather than its file
//! path: the id travels with the log, so a replica on a second machine and a
//! restored backup both look in the same place. Folders are 0700 and files 0600,
//! because these are the most sensitive bytes the program holds.
//!
//! A group server implements [`BlobStore`] against its own storage (see
//! `PERSONAL-TAX-SPEC.md`), and nothing above the trait changes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The largest file that can be attached.
///
/// Generous for a scanned statement and small enough that attaching the wrong
/// file (a video, a disk image) is refused rather than copied into storage that
/// will one day be synced.
pub const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("document storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("the file is empty")]
    Empty,
    #[error("the file is {size} bytes, over the {limit}-byte limit for an attached document")]
    TooLarge { size: u64, limit: u64 },
    #[error("\"{0}\" is not a SHA-256 digest")]
    InvalidDigest(String),
    #[error("the file for document {0} is not on this machine")]
    NotFound(String),
    #[error("the stored copy of {expected} is damaged: its contents hash to {actual}")]
    Corrupt { expected: String, actual: String },
    #[error("\"{0}\" cannot name a document folder")]
    InvalidLedgerId(String),
}

/// What storing some bytes produced: the key to get them back, and their size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub sha256: String,
    pub size_bytes: u64,
}

/// Somewhere document bytes are kept, addressed by their SHA-256.
///
/// The local folder today; a group server's storage later. Implementations must
/// never hand back bytes that do not hash to the digest asked for.
pub trait BlobStore {
    /// Keep these bytes. Storing bytes already held is not an error.
    fn put(&self, bytes: &[u8]) -> Result<BlobRef, BlobError>;
    /// The bytes whose SHA-256 is `sha256`, verified.
    fn get(&self, sha256: &str) -> Result<Vec<u8>, BlobError>;
    /// Whether the bytes are held here, without reading or verifying them.
    fn contains(&self, sha256: &str) -> bool;
}

/// Lowercase hex SHA-256, the form the log records.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `s` is a digest in the form the log records.
///
/// Lowercase only: two spellings of one digest would be two keys for one file.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Refuse a file before it is read into storage.
pub fn check_size(size: u64) -> Result<(), BlobError> {
    if size == 0 {
        Err(BlobError::Empty)
    } else if size > MAX_DOCUMENT_BYTES {
        Err(BlobError::TooLarge {
            size,
            limit: MAX_DOCUMENT_BYTES,
        })
    } else {
        Ok(())
    }
}

/// The identity of a set of books: the `company_id` its `CompanyCreated` event
/// carries — or, for books with none, the hash of the first event in their log.
///
/// Either is in the log, so every replica of the books and every restored copy
/// agrees on it — which a file path does not survive.
///
/// # Why there is a fallback
///
/// Books created on a group server begin with their accounts, not with a
/// `CompanyCreated` event, so they have no company id at all. Without an identity
/// they could not be linked to a person's return, nor keep documents. The first
/// event's hash is as fixed as a company id — a replica that disagreed about it
/// would fail verification — and is prefixed so it can never be mistaken for one.
pub fn ledger_id(conn: &Connection) -> Option<String> {
    let company: Option<String> = conn
        .query_row(
            "SELECT company_id FROM company
              WHERE company_id IS NOT NULL AND company_id != ''
              ORDER BY created_at_event LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    company.or_else(|| {
        conn.query_row("SELECT hash FROM events ORDER BY id LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .optional()
        .ok()
        .flatten()
        .map(|hash| format!("log-{}", hex::encode(hash)))
    })
}

/// The name a set of books goes by, for a link that has to say which books.
pub fn ledger_name(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT name FROM company ORDER BY created_at_event LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// What kind of file this is, from its first bytes where they say, and from its
/// name only for plain-text formats that have no signature.
///
/// The bytes win over the name because the name is whatever somebody typed: a
/// PDF called `statement.txt` is still a PDF, and anything that opens it later
/// should be told so.
pub fn sniff_media_type(bytes: &[u8], filename: &str) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return "image/png";
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    let extension = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("csv") => "text/csv",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// The name to record for an attached file: its last path component, with
/// control characters dropped and the length capped.
///
/// Display only. The stored file is named by its digest, so nothing here can
/// steer where anything is written.
pub fn display_filename(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    let clean = if clean.is_empty() { "document" } else { clean };
    clean.chars().take(255).collect()
}

/// Document bytes in a folder on this machine.
#[derive(Debug, Clone)]
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    /// A store rooted at an explicit folder, for tests and for callers that have
    /// already decided where the files go.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store for one set of books, under the standard data folder.
    pub fn for_ledger(ledger_id: &str) -> Result<Self, BlobError> {
        Self::for_ledger_under(&crate::registry::data_dir().join("documents"), ledger_id)
    }

    /// The store for one set of books under `base`.
    ///
    /// The id becomes a folder name, so it is held to characters that cannot
    /// climb out of `base` — it came from a log that other people write to.
    pub fn for_ledger_under(base: &Path, ledger_id: &str) -> Result<Self, BlobError> {
        let safe = !ledger_id.is_empty()
            && ledger_id.len() <= 128
            && ledger_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !safe {
            return Err(BlobError::InvalidLedgerId(ledger_id.to_string()));
        }
        Ok(Self::at(base.join(ledger_id)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.root.join("sha256").join(&sha256[..2]).join(sha256)
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, bytes: &[u8]) -> Result<BlobRef, BlobError> {
        let size_bytes = bytes.len() as u64;
        check_size(size_bytes)?;
        let sha256 = sha256_hex(bytes);
        let dest = self.path_for(&sha256);

        // Already here and intact: nothing to write. A damaged copy falls through
        // and is replaced, by bytes known to be right because they hash to the name.
        if let Ok(existing) = fs::read(&dest) {
            if sha256_hex(&existing) == sha256 {
                return Ok(BlobRef { sha256, size_bytes });
            }
        }

        let dir = dest.parent().expect("a blob path always has a parent");
        create_private_dir_all(dir)?;
        // Written beside the destination and renamed over it, so a crash leaves a
        // stray temporary file rather than a truncated blob under a real digest.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(".{sha256}.{}.{nonce}.tmp", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut file = open_private(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &dest)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(BlobRef { sha256, size_bytes })
    }

    fn get(&self, sha256: &str) -> Result<Vec<u8>, BlobError> {
        if !is_sha256_hex(sha256) {
            return Err(BlobError::InvalidDigest(sha256.to_string()));
        }
        let bytes = match fs::read(self.path_for(sha256)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound(sha256.to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        let actual = sha256_hex(&bytes);
        if actual != sha256 {
            return Err(BlobError::Corrupt {
                expected: sha256.to_string(),
                actual,
            });
        }
        Ok(bytes)
    }

    fn contains(&self, sha256: &str) -> bool {
        is_sha256_hex(sha256) && self.path_for(sha256).is_file()
    }
}

#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {

    /// Books a group server created begin with accounts, not a company, and still
    /// have an identity every copy of them agrees on; books with a company keep
    /// its id.
    #[test]
    fn books_without_a_company_are_known_by_their_first_event() {
        use crate::store::migrations::SchemaStore;
        let mut store = crate::store::event_store::EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        assert_eq!(super::ledger_id(store.connection()), None, "no events, no identity");
        crate::commands::account_commands::AccountCommands::new(&mut store, "u".to_string())
            .create_account(crate::commands::account_commands::CreateAccountCommand {
                account_type: crate::domain::AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            })
            .unwrap();
        let id = super::ledger_id(store.connection()).unwrap();
        assert!(id.starts_with("log-"), "{id}");
        assert_eq!(super::ledger_id(store.connection()).unwrap(), id, "stable");

        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('c', 'company-1', 'Co', 'USD', 1)",
                [],
            )
            .unwrap();
        assert_eq!(super::ledger_id(store.connection()).as_deref(), Some("company-1"));
    }
    use super::*;

    fn store() -> (LocalBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::for_ledger_under(dir.path(), "ledger-1").unwrap();
        (store, dir)
    }

    #[test]
    fn bytes_come_back_as_they_went_in() {
        let (store, _dir) = store();
        let stored = store.put(b"%PDF-1.7 a 1099-INT").unwrap();
        assert_eq!(stored.size_bytes, 19);
        assert!(is_sha256_hex(&stored.sha256));
        assert!(store.contains(&stored.sha256));
        assert_eq!(store.get(&stored.sha256).unwrap(), b"%PDF-1.7 a 1099-INT");
    }

    #[test]
    fn storing_the_same_bytes_twice_is_one_blob() {
        let (store, _dir) = store();
        let a = store.put(b"same").unwrap();
        let b = store.put(b"same").unwrap();
        assert_eq!(a, b);
        let files = fs::read_dir(store.path_for(&a.sha256).parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(files, 1, "no second copy and no temporary file left behind");
    }

    /// The whole point of addressing by content: a copy that has been altered
    /// is refused, not returned.
    #[test]
    fn a_damaged_copy_is_refused_and_a_fresh_put_repairs_it() {
        let (store, _dir) = store();
        let stored = store.put(b"original").unwrap();
        fs::write(store.path_for(&stored.sha256), b"tampered").unwrap();
        assert!(matches!(
            store.get(&stored.sha256),
            Err(BlobError::Corrupt { .. })
        ));
        store.put(b"original").unwrap();
        assert_eq!(store.get(&stored.sha256).unwrap(), b"original");
    }

    #[test]
    fn a_missing_file_says_so() {
        let (store, _dir) = store();
        let digest = sha256_hex(b"never stored");
        assert!(!store.contains(&digest));
        assert!(matches!(store.get(&digest), Err(BlobError::NotFound(_))));
    }

    #[test]
    fn empty_and_oversized_files_are_refused() {
        let (store, _dir) = store();
        assert!(matches!(store.put(b""), Err(BlobError::Empty)));
        assert!(matches!(
            check_size(MAX_DOCUMENT_BYTES + 1),
            Err(BlobError::TooLarge { .. })
        ));
        assert!(check_size(MAX_DOCUMENT_BYTES).is_ok());
    }

    #[test]
    fn a_digest_in_the_wrong_shape_is_refused() {
        let (store, _dir) = store();
        assert!(matches!(
            store.get("../../etc/passwd"),
            Err(BlobError::InvalidDigest(_))
        ));
        let upper = sha256_hex(b"x").to_uppercase();
        assert!(!is_sha256_hex(&upper), "one spelling per digest");
    }

    /// The ledger id comes out of a log other people write to, and becomes a
    /// folder name.
    #[test]
    fn a_ledger_id_cannot_climb_out_of_the_documents_folder() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "..", "../other", "a/b", "a\\b", "with space"] {
            assert!(
                LocalBlobStore::for_ledger_under(dir.path(), bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(LocalBlobStore::for_ledger_under(
            dir.path(),
            "700ebb5b-27fd-43be-b22c-d7d3406803f1"
        )
        .is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn files_and_folders_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let (store, _dir) = store();
        let stored = store.put(b"ssn inside").unwrap();
        let path = store.path_for(&stored.sha256);
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn the_media_type_comes_from_the_bytes_before_the_name() {
        assert_eq!(sniff_media_type(b"%PDF-1.4", "k1.txt"), "application/pdf");
        assert_eq!(
            sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0], "scan"),
            "image/jpeg"
        );
        assert_eq!(sniff_media_type(b"a,b\n1,2", "export.CSV"), "text/csv");
        assert_eq!(
            sniff_media_type(b"MZ", "setup.exe"),
            "application/octet-stream"
        );
    }

    #[test]
    fn a_recorded_filename_is_a_name_and_not_a_path() {
        assert_eq!(
            display_filename("/home/me/2025/1099-INT.pdf"),
            "1099-INT.pdf"
        );
        assert_eq!(display_filename("C:\\scans\\w2.pdf"), "w2.pdf");
        assert_eq!(display_filename("bad\u{0007}name.pdf"), "badname.pdf");
        assert_eq!(display_filename("   "), "document");
    }
}
