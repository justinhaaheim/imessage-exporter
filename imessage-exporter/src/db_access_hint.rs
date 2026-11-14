/*!
 Fork-owned (imessage-exporter-djf.3): an extra hint printed on macOS after upstream's
 "cannot open the Messages database" error.

 Upstream's message (`FULL_DISK_ACCESS` in `imessage-database/src/error/table.rs`) only says to
 give the terminal emulator Full Disk Access. A terminal that already has it can still fail to
 open `chat.db`, so this hint gives two actions that work whatever the cause. It states no cause.
 It lives in its own file, outside upstream-owned code, so upstream merges do not conflict with it.

 The copy command names the database that failed (djf.12 F9): the default macOS database in its
 `~/Library/Messages` form, any other database file by its path, and nothing for an iOS backup,
 whose Messages database is one file inside the backup, not something the exporter reads on its own.
 A path the hint cannot inspect gets a copy command only when it looks like a database file
 (djf.16 R5).
*/

use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
};

use clap::ArgMatches;
use imessage_database::{
    error::table::{TableConnectError, TableError},
    tables::table::DEFAULT_PATH_IOS,
    util::{dirs::default_db_path, platform::Platform},
};

use crate::app::{
    error::RuntimeError,
    options::{OPTION_PLATFORM, Options},
};

/// Copies the live database, with its `-wal` and `-shm` files, into a new folder.
///
/// `mktemp -d` makes a new folder every time, mode 700, so other local accounts cannot read the
/// copy (djf.16 R1). A folder reused from an earlier copy could keep that copy's `-wal` and
/// `-shm` files, and SQLite would apply that stale write-ahead log to the new copy, so the
/// export would read the earlier database. A reused folder could also keep another owner's mode
/// or be a planted symlink. The folder's path is kept in `D` for the export command, so both
/// lines must run in the same shell ([`SAME_WINDOW`]).
const COPY_DB: &str = r#"D=$(mktemp -d /tmp/imx.XXXXXX) && cp ~/Library/Messages/chat.db* "$D"/"#;
/// Runs the export against that copy.
const EXPORT_COPY: &str = r#"imessage-exporter -p "$D"/chat.db <your other options>"#;
/// The start of [`COPY_DB`] that makes the new folder, for a database at another path.
const MAKE_COPY_DIR: &str = "D=$(mktemp -d /tmp/imx.XXXXXX)";
/// Said before the two command lines, which share the folder path in `D`.
const SAME_WINDOW: &str = "Run these two lines in the same terminal window.";

/// What `main` knows about the Messages database the run was told to read, kept before
/// `Config::new` takes the options.
pub(crate) struct ConfiguredDb {
    /// The database file the exporter opens, or `None` when the options could not be parsed.
    path: Option<PathBuf>,
    /// True when the source is an iOS backup.
    ios_backup: bool,
}

impl ConfiguredDb {
    /// Read it from the parsed options when there are some. When parsing failed, only an
    /// explicit `--platform` on the command line says whether the source is an iOS backup.
    pub(crate) fn new(args: &ArgMatches, options: Option<&Options>) -> Self {
        let explicit_platform = args
            .get_one::<String>(OPTION_PLATFORM)
            .and_then(|platform| Platform::from_cli(platform));
        Self::from_parts(options, explicit_platform)
    }

    fn from_parts(options: Option<&Options>, explicit_platform: Option<Platform>) -> Self {
        match options {
            Some(options) => Self {
                path: Some(options.get_db_path()),
                ios_backup: options.platform == Platform::iOS,
            },
            None => Self {
                path: None,
                ios_backup: explicit_platform == Some(Platform::iOS),
            },
        }
    }
}

/// The database that failed, as far as the hint's copy command is concerned.
#[derive(Debug, PartialEq, Eq)]
enum FailedDb {
    /// The default macOS database, `~/Library/Messages/chat.db`.
    DefaultMacos,
    /// A database file at any other path.
    Other(PathBuf),
    /// The Messages database inside an iOS backup: the hint gives no copy command.
    IosBackup,
    /// A path that is not a database file, or may not be one (djf.16 R5): a folder, or a path
    /// the hint cannot inspect whose name does not end in `.db` or that lies inside
    /// `MobileSync/Backup`. The hint gives no copy command, because `cp` would copy nothing
    /// useful.
    NotADatabaseFile,
    /// No path is known: the hint gives no copy command.
    Unknown,
}

/// What the hint can tell about the failing path on disk (djf.16 R5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    /// A regular file.
    File,
    /// A folder, or anything else that is not a regular file.
    NotAFile,
    /// The path cannot be inspected, for example because macOS privacy protection blocks a
    /// folder above it.
    Unknown,
}

/// Print the hint to stderr after `why` when `why` means the database file could not be opened.
/// The macOS gate lives here, at the call site, so [`db_access_hint`] stays testable everywhere.
pub(crate) fn eprint_db_access_hint(why: &RuntimeError, configured: &ConfiguredDb) {
    if cfg!(target_os = "macos") && is_db_access_error(why) {
        let failed = failed_db(why, configured, default_db_path().as_deref(), path_kind);
        eprintln!("{}", db_access_hint(std::env::current_exe().ok(), &failed));
    }
}

/// Inspect `path` on disk. A failed inspection is [`PathKind::Unknown`], never a kind.
fn path_kind(path: &Path) -> PathKind {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => PathKind::File,
        Ok(_) => PathKind::NotAFile,
        Err(_) => PathKind::Unknown,
    }
}

/// True for the two errors that upstream ends with its Full Disk Access advice.
fn is_db_access_error(why: &RuntimeError) -> bool {
    matches!(
        why,
        RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::Permissions(_) | TableConnectError::Inaccessible(..)
        ))
    )
}

/// Find the database that failed. `Inaccessible` carries its path. `Permissions` does not: it
/// comes from opening the configured database, so that path is the one. `default_db` is the
/// default macOS database path, or `None` when the home directory is unknown. `kind_of`
/// inspects a path on disk ([`path_kind`] outside tests).
///
/// A path that cannot be inspected may be a folder: run without `--platform iOS`, an iOS backup
/// root inside a folder the exporter cannot search fails before its kind is known. Such a path
/// gets a copy command only when it looks like a database file (djf.16 R5).
fn failed_db(
    why: &RuntimeError,
    configured: &ConfiguredDb,
    default_db: Option<&Path>,
    kind_of: impl FnOnce(&Path) -> PathKind,
) -> FailedDb {
    if configured.ios_backup {
        return FailedDb::IosBackup;
    }
    let path = match why {
        RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::Inaccessible(path, _),
        )) => Some(path.as_path()),
        _ => configured.path.as_deref(),
    };
    match path {
        // Platform::determine reports the backup's Messages database when it cannot inspect it.
        Some(path) if path.ends_with(DEFAULT_PATH_IOS) => FailedDb::IosBackup,
        Some(path) if default_db == Some(path) => FailedDb::DefaultMacos,
        Some(path) if path.file_name().is_some() => match kind_of(path) {
            PathKind::File => FailedDb::Other(path.to_path_buf()),
            PathKind::Unknown if looks_like_a_database_file(path) => {
                FailedDb::Other(path.to_path_buf())
            }
            PathKind::Unknown | PathKind::NotAFile => FailedDb::NotADatabaseFile,
        },
        _ => FailedDb::Unknown,
    }
}

/// True when `path`'s file name ends in `.db` and the path is not inside an iOS backup folder
/// (`MobileSync/Backup`), whose files are not databases the exporter reads on their own.
fn looks_like_a_database_file(path: &Path) -> bool {
    let db_name = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("db"));
    let parts: Vec<_> = path.components().map(|part| part.as_os_str()).collect();
    let in_backup = parts.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("MobileSync") && pair[1].eq_ignore_ascii_case("Backup")
    });
    db_name && !in_backup
}

/// Build the hint text. `exe_path` is the running binary's path from
/// [`std::env::current_exe`], or `None` when that call failed.
/// An empty path counts as `None`, so the hint never prints an empty path.
fn db_access_hint(exe_path: Option<PathBuf>, failed: &FailedDb) -> String {
    let add_binary = match exe_path.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => format!(
            "If your terminal already has Full Disk Access and this still fails, add this program itself in System Settings > Privacy & Security > Full Disk Access:\n    {}",
            path.display()
        ),
        None => String::from(
            "If your terminal already has Full Disk Access and this still fails, add this program itself (the imessage-exporter binary you ran) in System Settings > Privacy & Security > Full Disk Access.",
        ),
    };
    match copy_commands(failed) {
        Some((copy, export)) => format!(
            "{add_binary}\nOr copy the database to a folder you can read and point at the copy:\n{SAME_WINDOW}\n    {copy}\n    {export}"
        ),
        None => add_binary,
    }
}

/// The copy command and the export command for the database that failed, or `None` when the
/// hint has no copy command to give.
fn copy_commands(failed: &FailedDb) -> Option<(String, String)> {
    match failed {
        FailedDb::DefaultMacos => Some((String::from(COPY_DB), String::from(EXPORT_COPY))),
        FailedDb::Other(path) => {
            let file_name = path.file_name()?.to_string_lossy();
            let source = path.to_string_lossy();
            Some((
                format!("{MAKE_COPY_DIR} && cp {}* \"$D\"/", shell_quote(&source)),
                format!(
                    "imessage-exporter -p \"$D\"/{} <your other options>",
                    shell_quote(&file_name)
                ),
            ))
        }
        FailedDb::IosBackup | FailedDb::NotADatabaseFile | FailedDb::Unknown => None,
    }
}

/// Quote `text` for a POSIX shell unless every character is plainly safe there. A `*` written
/// right after the closing quote still globs, so `'<path>'*` matches the `-wal` and `-shm` files.
fn shell_quote(text: &str) -> Cow<'_, str> {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c));
    if plain {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("'{}'", text.replace('\'', r"'\''")))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Error as IoError, ErrorKind},
        path::{Path, PathBuf},
    };

    use imessage_database::{
        error::table::{TableConnectError, TableError},
        rusqlite,
        tables::table::DEFAULT_PATH_IOS,
        util::platform::Platform,
    };

    use super::{
        ConfiguredDb, FailedDb, PathKind, db_access_hint, failed_db, is_db_access_error, path_kind,
        shell_quote,
    };
    use crate::app::{
        error::RuntimeError, export_type::ExportType, options::Options, test_dir::unique_test_dir,
    };

    const COPY_LINE: &str =
        r#"    D=$(mktemp -d /tmp/imx.XXXXXX) && cp ~/Library/Messages/chat.db* "$D"/"#;
    const EXPORT_LINE: &str = r#"    imessage-exporter -p "$D"/chat.db <your other options>"#;
    const SAME_WINDOW_LINE: &str = "Run these two lines in the same terminal window.";
    const DEFAULT_DB: &str = "/Users/someone/Library/Messages/chat.db";

    fn inaccessible(path: &str) -> RuntimeError {
        RuntimeError::DatabaseError(TableError::CannotConnect(TableConnectError::Inaccessible(
            PathBuf::from(path),
            IoError::from(ErrorKind::PermissionDenied),
        )))
    }

    fn permissions() -> RuntimeError {
        RuntimeError::DatabaseError(TableError::CannotConnect(TableConnectError::Permissions(
            rusqlite::Error::InvalidQuery,
        )))
    }

    fn configured(path: &str, platform: Platform) -> ConfiguredDb {
        let mut options = Options::fake_options(ExportType::Timeline);
        options.db_path = PathBuf::from(path);
        options.platform = platform;
        ConfiguredDb::from_parts(Some(&options), None)
    }

    /// Inspection that finds a regular file, as for a database whose folder is readable.
    fn as_file(_: &Path) -> PathKind {
        PathKind::File
    }

    /// Inspection that fails, as for a path inside a folder the exporter cannot search.
    fn uninspectable(_: &Path) -> PathKind {
        PathKind::Unknown
    }

    #[test]
    fn hint_with_path_names_that_exact_path() {
        let exe = PathBuf::from("/Users/someone/.cargo/bin/imessage-exporter");
        let hint = db_access_hint(Some(exe), &FailedDb::DefaultMacos);

        assert!(
            hint.lines()
                .any(|line| line == "    /Users/someone/.cargo/bin/imessage-exporter"),
            "hint lacks the binary path on its own line:\n{hint}"
        );
        assert!(hint.contains("System Settings > Privacy & Security > Full Disk Access"));
        assert!(hint.contains("cp ~/Library/Messages/chat.db*"));
        assert!(hint.contains(r#"-p "$D"/chat.db"#));
        assert!(hint.lines().any(|line| line == COPY_LINE));
        assert!(hint.lines().any(|line| line == EXPORT_LINE));
        assert!(!hint.contains("the imessage-exporter binary you ran"));
    }

    #[test]
    fn copy_goes_to_a_new_private_folder_every_time() {
        // djf.16 R1: a reused /tmp/imx kept an earlier copy's -wal and -shm, and SQLite
        // applied that stale log to the new copy. mktemp -d makes a new folder, mode 700.
        let hint = db_access_hint(None, &FailedDb::DefaultMacos);
        assert!(hint.contains("mktemp -d /tmp/imx.XXXXXX"), "got:\n{hint}");
        assert!(hint.contains(r#""$D"/"#), "got:\n{hint}");
        assert!(
            !hint.contains("/tmp/imx/"),
            "hint reuses a fixed folder:\n{hint}"
        );
        assert!(!hint.contains("mkdir"), "got:\n{hint}");

        // The same-window line comes right before the two command lines.
        let lines: Vec<&str> = hint.lines().collect();
        let at = lines
            .iter()
            .position(|line| *line == SAME_WINDOW_LINE)
            .unwrap_or_else(|| panic!("hint lacks the same-window line:\n{hint}"));
        assert_eq!(lines[at + 1..], [COPY_LINE, EXPORT_LINE], "got:\n{hint}");
    }

    #[test]
    fn hint_without_path_names_the_binary_in_words() {
        let hint = db_access_hint(None, &FailedDb::DefaultMacos);

        assert!(hint.contains("the imessage-exporter binary you ran"));
        assert!(hint.contains("cp ~/Library/Messages/chat.db*"));
        assert!(hint.contains(r#"-p "$D"/chat.db"#));
        // No line is left for a path: no line ends in the colon that would introduce one,
        // and no line is blank or only indentation.
        assert!(
            !hint.contains("Full Disk Access:"),
            "hint introduces a path it does not have:\n{hint}"
        );
        assert!(
            hint.lines().all(|line| !line.trim().is_empty()),
            "hint has an empty line:\n{hint}"
        );
    }

    #[test]
    fn hint_with_empty_path_is_the_same_as_no_path() {
        assert_eq!(
            db_access_hint(Some(PathBuf::new()), &FailedDb::DefaultMacos),
            db_access_hint(None, &FailedDb::DefaultMacos)
        );
    }

    #[test]
    fn hint_applies_to_inaccessible_and_permissions_only() {
        let not_a_database = RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::NotADatabase(PathBuf::from("/tmp/chat.db")),
        ));
        let query_error =
            RuntimeError::DatabaseError(TableError::QueryError(rusqlite::Error::InvalidQuery));
        let disk_error = RuntimeError::DiskError(IoError::from(ErrorKind::PermissionDenied));

        assert!(is_db_access_error(&inaccessible("/tmp/chat.db")));
        assert!(is_db_access_error(&permissions()));
        assert!(!is_db_access_error(&not_a_database));
        assert!(!is_db_access_error(&query_error));
        assert!(!is_db_access_error(&disk_error));
    }

    #[test]
    fn copy_command_names_a_custom_database_by_its_path() {
        // djf.12 F9: a database given with -p is copied from where it is, not from
        // ~/Library/Messages, and the export points at the copy's own file name.
        let custom = "/Volumes/Archive/old-messages.db";
        let failed = failed_db(
            &inaccessible(custom),
            &configured(custom, Platform::macOS),
            Some(Path::new(DEFAULT_DB)),
            as_file,
        );
        assert_eq!(failed, FailedDb::Other(PathBuf::from(custom)));

        let hint = db_access_hint(None, &failed);
        assert!(
            hint.lines().any(|line| line
                == r#"    D=$(mktemp -d /tmp/imx.XXXXXX) && cp /Volumes/Archive/old-messages.db* "$D"/"#),
            "hint does not copy the custom database:\n{hint}"
        );
        assert!(
            hint.lines().any(|line| line
                == r#"    imessage-exporter -p "$D"/old-messages.db <your other options>"#),
            "hint does not export the copy:\n{hint}"
        );
        assert!(hint.lines().any(|line| line == SAME_WINDOW_LINE));
        assert!(!hint.contains("~/Library/Messages"));
    }

    #[test]
    fn copy_command_quotes_a_path_the_shell_would_split() {
        let custom = "/Users/someone/Old Mac/chat.db";
        let failed = FailedDb::Other(PathBuf::from(custom));
        let hint = db_access_hint(None, &failed);
        assert!(
            hint.lines().any(|line| line
                == r#"    D=$(mktemp -d /tmp/imx.XXXXXX) && cp '/Users/someone/Old Mac/chat.db'* "$D"/"#),
            "hint does not quote the path:\n{hint}"
        );
        assert!(hint.lines().any(|line| line == EXPORT_LINE));

        // The copy's own file name is quoted too.
        let hint = db_access_hint(None, &FailedDb::Other(PathBuf::from("/data/it's.db")));
        assert!(
            hint.lines()
                .any(|line| line
                    == r#"    imessage-exporter -p "$D"/'it'\''s.db' <your other options>"#),
            "hint does not quote the file name:\n{hint}"
        );

        assert_eq!(shell_quote("/tmp/imx/chat.db"), "/tmp/imx/chat.db");
        assert_eq!(shell_quote("it's.db"), r"'it'\''s.db'");
        assert_eq!(shell_quote("~/chat.db"), "'~/chat.db'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn permissions_error_names_the_configured_database() {
        // Permissions carries no path: the database that failed is the one the run opened.
        let default_db = Some(Path::new(DEFAULT_DB));
        assert_eq!(
            failed_db(
                &permissions(),
                &configured("/data/chat.db", Platform::macOS),
                default_db,
                as_file
            ),
            FailedDb::Other(PathBuf::from("/data/chat.db"))
        );
        assert_eq!(
            failed_db(
                &permissions(),
                &configured(DEFAULT_DB, Platform::macOS),
                default_db,
                as_file
            ),
            FailedDb::DefaultMacos
        );
        // Before the options are parsed there is no configured path to name.
        assert_eq!(
            failed_db(
                &permissions(),
                &ConfiguredDb::from_parts(None, None),
                default_db,
                as_file
            ),
            FailedDb::Unknown
        );
    }

    #[test]
    fn default_database_keeps_the_library_form() {
        let failed = failed_db(
            &inaccessible(DEFAULT_DB),
            &ConfiguredDb::from_parts(None, None),
            Some(Path::new(DEFAULT_DB)),
            as_file,
        );
        assert_eq!(failed, FailedDb::DefaultMacos);
        // An unknown home directory makes no path the default one.
        assert_eq!(
            failed_db(
                &inaccessible(DEFAULT_DB),
                &ConfiguredDb::from_parts(None, None),
                None,
                as_file
            ),
            FailedDb::Other(PathBuf::from(DEFAULT_DB))
        );
    }

    #[test]
    fn ios_backup_gets_no_copy_command() {
        let default_db = Some(Path::new(DEFAULT_DB));
        // The backup's own Messages database, as Platform::determine reports it.
        let nested = format!("/Users/someone/Backup/abc/{DEFAULT_PATH_IOS}");
        assert_eq!(
            failed_db(
                &inaccessible(&nested),
                &ConfiguredDb::from_parts(None, None),
                default_db,
                as_file
            ),
            FailedDb::IosBackup
        );
        // The backup root itself, with --platform iOS given.
        assert_eq!(
            failed_db(
                &inaccessible("/Users/someone/Backup/abc"),
                &ConfiguredDb::from_parts(None, Some(Platform::iOS)),
                default_db,
                as_file
            ),
            FailedDb::IosBackup
        );
        // Parsed options that say iOS.
        assert_eq!(
            failed_db(
                &permissions(),
                &configured("/Users/someone/Backup/abc", Platform::iOS),
                default_db,
                as_file
            ),
            FailedDb::IosBackup
        );

        let hint = db_access_hint(None, &FailedDb::IosBackup);
        assert!(hint.contains("System Settings > Privacy & Security > Full Disk Access"));
        assert!(!hint.contains("copy"), "iOS hint has a copy step:\n{hint}");
        assert!(
            !hint.contains("cp "),
            "iOS hint has a copy command:\n{hint}"
        );
        assert_eq!(hint.lines().count(), 1);
    }

    #[test]
    fn uninspectable_backup_root_gets_no_copy_command() {
        // djf.16 R5: run without --platform iOS, a backup root inside a folder the exporter
        // cannot search fails before its kind is known. It is not a database file.
        let default_db = Some(Path::new(DEFAULT_DB));
        let root = "/Volumes/locked/00008030-001A2B3C4D5E";
        let failed = failed_db(
            &inaccessible(root),
            &ConfiguredDb::from_parts(None, None),
            default_db,
            uninspectable,
        );
        assert_eq!(failed, FailedDb::NotADatabaseFile);

        let hint = db_access_hint(None, &failed);
        assert!(hint.contains("System Settings > Privacy & Security > Full Disk Access"));
        assert!(!hint.contains("cp "), "hint has a copy command:\n{hint}");
        assert_eq!(hint.lines().count(), 1, "got:\n{hint}");
    }

    #[test]
    fn uninspectable_path_gets_a_copy_command_only_when_it_looks_like_a_database_file() {
        let default_db = Some(Path::new(DEFAULT_DB));
        let unknown = |path: &str| {
            failed_db(
                &inaccessible(path),
                &ConfiguredDb::from_parts(None, None),
                default_db,
                uninspectable,
            )
        };
        for path in ["/Volumes/locked/old-messages.db", "/Volumes/locked/CHAT.DB"] {
            assert_eq!(
                unknown(path),
                FailedDb::Other(PathBuf::from(path)),
                "{path}"
            );
        }
        for path in [
            "/Volumes/locked/old-messages.sqlite",
            "/Volumes/locked/chat.db-wal",
            "/Volumes/locked/db",
            "/Users/someone/Library/Application Support/MobileSync/Backup/abc/chat.db",
            "/Users/someone/Library/Application Support/mobilesync/backup/chat.db",
        ] {
            assert_eq!(unknown(path), FailedDb::NotADatabaseFile, "{path}");
        }
    }

    #[test]
    fn inspected_path_gets_a_copy_command_only_when_it_is_a_file() {
        let default_db = Some(Path::new(DEFAULT_DB));
        let with_kind = |path: &str, kind: PathKind| {
            failed_db(
                &inaccessible(path),
                &ConfiguredDb::from_parts(None, None),
                default_db,
                |_: &Path| kind,
            )
        };
        // A file whose permissions forbid reading it: any name can be copied.
        assert_eq!(
            with_kind("/data/messages.sqlite", PathKind::File),
            FailedDb::Other(PathBuf::from("/data/messages.sqlite"))
        );
        // A folder is never copied with cp, whatever its name.
        assert_eq!(
            with_kind("/data/folder.db", PathKind::NotAFile),
            FailedDb::NotADatabaseFile
        );
    }

    #[test]
    fn path_kind_inspects_the_disk_and_reports_a_failed_inspection_as_unknown() {
        let dir = unique_test_dir("db-access-hint-path-kind");
        let file = dir.join("chat.db");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(path_kind(&file), PathKind::File);
        assert_eq!(path_kind(&dir), PathKind::NotAFile);
        // A path below a regular file can never be inspected.
        assert_eq!(path_kind(&file.join("below")), PathKind::Unknown);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
