/*!
 Fork-owned (imessage-exporter-djf.3): an extra hint printed on macOS after upstream's
 "cannot open the Messages database" error.

 Upstream's message (`FULL_DISK_ACCESS` in `imessage-database/src/error/table.rs`) only says to
 give the terminal emulator Full Disk Access. A terminal that already has it can still fail to
 open `chat.db`, so this hint gives two actions that work whatever the cause. It states no cause.
 It lives in its own file, outside upstream-owned code, so upstream merges do not conflict with it.
*/

use std::path::PathBuf;

use imessage_database::error::table::{TableConnectError, TableError};

use crate::app::error::RuntimeError;

/// Copies the live database, with its `-wal` and `-shm` files, to a folder the user can read.
const COPY_DB: &str = "mkdir -p /tmp/imx && cp ~/Library/Messages/chat.db* /tmp/imx/";
/// Runs the export against that copy.
const EXPORT_COPY: &str = "imessage-exporter -p /tmp/imx/chat.db <your other options>";

/// Print the hint to stderr after `why` when `why` means the database file could not be opened.
/// The macOS gate lives here, at the call site, so [`db_access_hint`] stays testable everywhere.
pub(crate) fn eprint_db_access_hint(why: &RuntimeError) {
    if cfg!(target_os = "macos") && is_db_access_error(why) {
        eprintln!("{}", db_access_hint(std::env::current_exe().ok()));
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

/// Build the hint text. `exe_path` is the running binary's path from
/// [`std::env::current_exe`], or `None` when that call failed.
/// An empty path counts as `None`, so the hint never prints an empty path.
fn db_access_hint(exe_path: Option<PathBuf>) -> String {
    let add_binary = match exe_path.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => format!(
            "If your terminal already has Full Disk Access and this still fails, add this program itself in System Settings > Privacy & Security > Full Disk Access:\n    {}",
            path.display()
        ),
        None => String::from(
            "If your terminal already has Full Disk Access and this still fails, add this program itself (the imessage-exporter binary you ran) in System Settings > Privacy & Security > Full Disk Access.",
        ),
    };
    format!(
        "{add_binary}\nOr copy the database to a folder you can read and point at the copy:\n    {COPY_DB}\n    {EXPORT_COPY}"
    )
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Error as IoError, ErrorKind},
        path::PathBuf,
    };

    use imessage_database::{
        error::table::{TableConnectError, TableError},
        rusqlite,
    };

    use super::{db_access_hint, is_db_access_error};
    use crate::app::error::RuntimeError;

    const COPY_LINE: &str = "    mkdir -p /tmp/imx && cp ~/Library/Messages/chat.db* /tmp/imx/";
    const EXPORT_LINE: &str = "    imessage-exporter -p /tmp/imx/chat.db <your other options>";

    #[test]
    fn hint_with_path_names_that_exact_path() {
        let exe = PathBuf::from("/Users/someone/.cargo/bin/imessage-exporter");
        let hint = db_access_hint(Some(exe));

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
        let hint = db_access_hint(None);

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
        assert_eq!(db_access_hint(Some(PathBuf::new())), db_access_hint(None));
    }

    #[test]
    fn hint_applies_to_inaccessible_and_permissions_only() {
        let inaccessible = RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::Inaccessible(
                PathBuf::from("/tmp/chat.db"),
                IoError::from(ErrorKind::PermissionDenied),
            ),
        ));
        let permissions = RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::Permissions(rusqlite::Error::InvalidQuery),
        ));
        let not_a_database = RuntimeError::DatabaseError(TableError::CannotConnect(
            TableConnectError::NotADatabase(PathBuf::from("/tmp/chat.db")),
        ));
        let query_error =
            RuntimeError::DatabaseError(TableError::QueryError(rusqlite::Error::InvalidQuery));
        let disk_error = RuntimeError::DiskError(IoError::from(ErrorKind::PermissionDenied));

        assert!(is_db_access_error(&inaccessible));
        assert!(is_db_access_error(&permissions));
        assert!(!is_db_access_error(&not_a_database));
        assert!(!is_db_access_error(&query_error));
        assert!(!is_db_access_error(&disk_error));
    }
}
