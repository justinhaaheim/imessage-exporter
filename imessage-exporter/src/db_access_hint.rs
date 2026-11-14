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
*/

use std::{
    borrow::Cow,
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

/// Copies the live database, with its `-wal` and `-shm` files, to a folder the user can read.
/// `-m 700` creates the folder private to the user, so other local accounts cannot read the
/// copy. It sets the mode only when it creates the folder; an existing `/tmp/imx` keeps its mode.
const COPY_DB: &str = "mkdir -p -m 700 /tmp/imx && cp ~/Library/Messages/chat.db* /tmp/imx/";
/// Runs the export against that copy.
const EXPORT_COPY: &str = "imessage-exporter -p /tmp/imx/chat.db <your other options>";
/// The folder [`COPY_DB`] copies to, for a database at another path.
const COPY_DIR: &str = "/tmp/imx";

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
    /// No path is known: the hint gives no copy command.
    Unknown,
}

/// Print the hint to stderr after `why` when `why` means the database file could not be opened.
/// The macOS gate lives here, at the call site, so [`db_access_hint`] stays testable everywhere.
pub(crate) fn eprint_db_access_hint(why: &RuntimeError, configured: &ConfiguredDb) {
    if cfg!(target_os = "macos") && is_db_access_error(why) {
        let failed = failed_db(why, configured, default_db_path().as_deref());
        eprintln!("{}", db_access_hint(std::env::current_exe().ok(), &failed));
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
/// default macOS database path, or `None` when the home directory is unknown.
fn failed_db(why: &RuntimeError, configured: &ConfiguredDb, default_db: Option<&Path>) -> FailedDb {
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
        Some(path) if path.file_name().is_some() => FailedDb::Other(path.to_path_buf()),
        _ => FailedDb::Unknown,
    }
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
            "{add_binary}\nOr copy the database to a folder you can read and point at the copy:\n    {copy}\n    {export}"
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
            let copy = format!("{COPY_DIR}/{file_name}");
            Some((
                format!(
                    "mkdir -p -m 700 {COPY_DIR} && cp {}* {COPY_DIR}/",
                    shell_quote(&source)
                ),
                format!(
                    "imessage-exporter -p {} <your other options>",
                    shell_quote(&copy)
                ),
            ))
        }
        FailedDb::IosBackup | FailedDb::Unknown => None,
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
        ConfiguredDb, FailedDb, db_access_hint, failed_db, is_db_access_error, shell_quote,
    };
    use crate::app::{error::RuntimeError, export_type::ExportType, options::Options};

    const COPY_LINE: &str =
        "    mkdir -p -m 700 /tmp/imx && cp ~/Library/Messages/chat.db* /tmp/imx/";
    const EXPORT_LINE: &str = "    imessage-exporter -p /tmp/imx/chat.db <your other options>";
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
        assert!(hint.contains("-p /tmp/imx/chat.db"));
        assert!(hint.lines().any(|line| line == COPY_LINE));
        assert!(hint.lines().any(|line| line == EXPORT_LINE));
        assert!(!hint.contains("the imessage-exporter binary you ran"));
    }

    #[test]
    fn hint_without_path_names_the_binary_in_words() {
        let hint = db_access_hint(None, &FailedDb::DefaultMacos);

        assert!(hint.contains("the imessage-exporter binary you ran"));
        assert!(hint.contains("cp ~/Library/Messages/chat.db*"));
        assert!(hint.contains("-p /tmp/imx/chat.db"));
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
        );
        assert_eq!(failed, FailedDb::Other(PathBuf::from(custom)));

        let hint = db_access_hint(None, &failed);
        assert!(
            hint.lines().any(|line| line
                == "    mkdir -p -m 700 /tmp/imx && cp /Volumes/Archive/old-messages.db* /tmp/imx/"),
            "hint does not copy the custom database:\n{hint}"
        );
        assert!(
            hint.lines().any(|line| line
                == "    imessage-exporter -p /tmp/imx/old-messages.db <your other options>"),
            "hint does not export the copy:\n{hint}"
        );
        assert!(!hint.contains("~/Library/Messages"));
    }

    #[test]
    fn copy_command_quotes_a_path_the_shell_would_split() {
        let custom = "/Users/someone/Old Mac/chat.db";
        let failed = FailedDb::Other(PathBuf::from(custom));
        let hint = db_access_hint(None, &failed);
        assert!(
            hint.lines().any(|line| line
                == "    mkdir -p -m 700 /tmp/imx && cp '/Users/someone/Old Mac/chat.db'* /tmp/imx/"),
            "hint does not quote the path:\n{hint}"
        );
        assert!(hint.lines().any(|line| line == EXPORT_LINE));

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
                default_db
            ),
            FailedDb::Other(PathBuf::from("/data/chat.db"))
        );
        assert_eq!(
            failed_db(
                &permissions(),
                &configured(DEFAULT_DB, Platform::macOS),
                default_db
            ),
            FailedDb::DefaultMacos
        );
        // Before the options are parsed there is no configured path to name.
        assert_eq!(
            failed_db(
                &permissions(),
                &ConfiguredDb::from_parts(None, None),
                default_db
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
        );
        assert_eq!(failed, FailedDb::DefaultMacos);
        // An unknown home directory makes no path the default one.
        assert_eq!(
            failed_db(
                &inaccessible(DEFAULT_DB),
                &ConfiguredDb::from_parts(None, None),
                None
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
                default_db
            ),
            FailedDb::IosBackup
        );
        // The backup root itself, with --platform iOS given.
        assert_eq!(
            failed_db(
                &inaccessible("/Users/someone/Backup/abc"),
                &ConfiguredDb::from_parts(None, Some(Platform::iOS)),
                default_db
            ),
            FailedDb::IosBackup
        );
        // Parsed options that say iOS.
        assert_eq!(
            failed_db(
                &permissions(),
                &configured("/Users/someone/Backup/abc", Platform::iOS),
                default_db
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
}
