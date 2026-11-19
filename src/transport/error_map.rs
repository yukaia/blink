//! Map transport-library errors to the typed `BlinkError` surface.
//!
//! [`map_sftp`] and [`map_ftp`] classify a library error once, in one
//! place, so call sites thread only the `op` and `path` context and get
//! back the variant callers act on: [`BlinkError::NotFound`] (the path
//! isn't there), [`BlinkError::Permission`], [`BlinkError::Disconnected`]
//! (the connection is gone or out of step: reconnect, and a transfer gets
//! one retry), [`BlinkError::AuthFailed`], and [`BlinkError::Transport`]
//! for the genuinely unclassifiable. A walk can then bail on a dropped
//! connection rather than march on against half a tree, and an existence
//! check can tell "not there" from "not allowed to look".
//!
//! Every message keeps the server's own words where it gave any: they are
//! often the only clue to what went wrong.

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use suppaftp::{FtpError, Status as FtpStatus};

use crate::error::BlinkError;

/// Classify an `russh_sftp` client error against `(op, path)` context.
///
/// - `Status(NoSuchFile)` → [`BlinkError::NotFound`]
/// - `Status(PermissionDenied)` → [`BlinkError::Permission`]
/// - `Status(NoConnection | ConnectionLost)`, `IO(_)`, `Timeout` →
///   [`BlinkError::Disconnected`]
/// - `UnexpectedPacket`, `UnexpectedBehavior(_)` →
///   [`BlinkError::Disconnected`]: the channel is out of step with the
///   server, and every request after it is suspect. Reported as a plain
///   error, the browsing connection carried on using it.
/// - Everything else → [`BlinkError::Transport`].
///
/// The server's message follows `op path` in every case.
pub fn map_sftp(op: &str, path: &str, err: SftpError) -> BlinkError {
    let detail = format!("{op} {path}: {err}");
    match &err {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => BlinkError::not_found(detail),
            StatusCode::PermissionDenied => BlinkError::permission(detail),
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                BlinkError::disconnected(detail)
            }
            _ => BlinkError::transport(detail),
        },
        SftpError::IO(_)
        | SftpError::Timeout
        | SftpError::UnexpectedPacket
        | SftpError::UnexpectedBehavior(_) => BlinkError::disconnected(detail),
        SftpError::Limited(_) => BlinkError::transport(detail),
    }
}

/// Classify a `suppaftp` error against `(op, path)` context.
///
/// - `ConnectionError(_)` / `SecureError(_)` → [`BlinkError::Disconnected`]
/// - `BadResponse` and `UnexpectedResponse(421 NotAvailable)` →
///   [`BlinkError::Disconnected`]. A reply that cannot be parsed leaves
///   the control connection out of step, so it is as good as gone; and
///   421 is how a server says it is about to close it, often for idle
///   time. (A connection the server closed before replying comes back as
///   `ConnectionError`; before suppaftp 12.1.1 it came back as
///   `BadResponse`.)
/// - `425` (can't open data connection) and `426` (connection closed,
///   transfer aborted) → [`BlinkError::Disconnected`]: a lost data
///   connection, usually a network blip, which a transfer gets a retry
///   for. Classed as a plain error, a blip mid-transfer failed the job.
/// - `550 FileUnavailable` → see [`classify_550`].
/// - `530 NotLoggedIn` → [`BlinkError::AuthFailed`].
/// - Everything else → [`BlinkError::Transport`].
///
/// The server's reply follows `op path` in every case.
pub fn map_ftp(op: &str, path: &str, err: FtpError) -> BlinkError {
    let detail = format!("{op} {path}: {err}");
    match &err {
        FtpError::ConnectionError(_) | FtpError::SecureError(_) | FtpError::BadResponse => {
            BlinkError::disconnected(detail)
        }
        FtpError::UnexpectedResponse(r) => match r.status {
            FtpStatus::NotAvailable
            | FtpStatus::CannotOpenDataConnection
            | FtpStatus::TransferAborted => BlinkError::disconnected(detail),
            FtpStatus::FileUnavailable => classify_550(&r.body, detail),
            FtpStatus::NotLoggedIn => BlinkError::auth(detail),
            _ => BlinkError::transport(detail),
        },
        _ => BlinkError::transport(detail),
    }
}

/// A `550`, which FTP uses both for "no such file" and for "no access".
///
/// Told apart by the server's reply text, as the servers in use word them
/// (`Permission denied`, IIS's `Access is denied.`, `Operation not
/// permitted`) — and it matters: as `NotFound`, a folder blink wasn't
/// allowed to list read as empty, and the upload overwrite check let
/// files in it be replaced without asking. Text that says neither (`550
/// Failed to change directory.`) is taken as not found, so an upload to a
/// server with terse replies still works.
///
/// Not-found wording is looked for first: "access" also turns up in
/// `Can't access /x: No such file or directory`.
fn classify_550(body: &[u8], detail: String) -> BlinkError {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    let missing = has(&[
        "no such",
        "not found",
        "cannot find",
        "can't find",
        "not exist",
        "n't exist",
    ]);
    let denied = has(&[
        "denied",
        "permission",
        "not permitted",
        "not allowed",
        "access",
    ]);
    if denied && !missing {
        BlinkError::permission(detail)
    } else {
        BlinkError::not_found(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status as SftpStatus;
    use suppaftp::types::Response;

    fn sftp_status(code: StatusCode) -> SftpError {
        SftpError::Status(SftpStatus {
            id: 1,
            status_code: code,
            error_message: "test".into(),
            language_tag: String::new(),
        })
    }

    fn ftp_response(status: FtpStatus) -> FtpError {
        FtpError::UnexpectedResponse(Response {
            status,
            body: b"test".to_vec(),
        })
    }

    #[test]
    fn sftp_no_such_file_is_not_found() {
        let e = map_sftp("open", "/x", sftp_status(StatusCode::NoSuchFile));
        assert!(matches!(e, BlinkError::NotFound(_)), "{e:?}");
    }

    #[test]
    fn sftp_permission_denied_is_permission() {
        let e = map_sftp("read", "/x", sftp_status(StatusCode::PermissionDenied));
        assert!(matches!(e, BlinkError::Permission(_)), "{e:?}");
    }

    #[test]
    fn sftp_connection_lost_is_disconnected() {
        let e = map_sftp("read", "/x", sftp_status(StatusCode::ConnectionLost));
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn sftp_no_connection_is_disconnected() {
        let e = map_sftp("read", "/x", sftp_status(StatusCode::NoConnection));
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn sftp_io_is_disconnected() {
        let e = map_sftp("read", "/x", SftpError::IO("broken pipe".into()));
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn sftp_timeout_is_disconnected() {
        let e = map_sftp("read", "/x", SftpError::Timeout);
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn sftp_failure_falls_back_to_transport() {
        let e = map_sftp("read", "/x", sftp_status(StatusCode::Failure));
        assert!(matches!(e, BlinkError::Transport(_)), "{e:?}");
    }

    #[test]
    fn sftp_op_unsupported_falls_back_to_transport() {
        let e = map_sftp("ext", "/x", sftp_status(StatusCode::OpUnsupported));
        assert!(matches!(e, BlinkError::Transport(_)), "{e:?}");
    }

    #[test]
    fn ftp_file_unavailable_is_not_found() {
        let e = map_ftp("list", "/x", ftp_response(FtpStatus::FileUnavailable));
        assert!(matches!(e, BlinkError::NotFound(_)), "{e:?}");
    }

    #[test]
    fn ftp_not_logged_in_is_auth_failed() {
        let e = map_ftp("list", "/x", ftp_response(FtpStatus::NotLoggedIn));
        assert!(matches!(e, BlinkError::AuthFailed(_)), "{e:?}");
    }

    #[test]
    fn ftp_connection_error_is_disconnected() {
        let e = map_ftp(
            "list",
            "/x",
            FtpError::ConnectionError(std::io::Error::other("eof")),
        );
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn ftp_secure_error_is_disconnected() {
        let e = map_ftp("list", "/x", FtpError::SecureError("tls".into()));
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn ftp_bad_response_is_disconnected() {
        let e = map_ftp("list", "/x", FtpError::BadResponse);
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn ftp_not_available_is_disconnected() {
        let e = map_ftp("list", "/x", ftp_response(FtpStatus::NotAvailable));
        assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
    }

    #[test]
    fn ftp_temporary_error_falls_back_to_transport() {
        let e = map_ftp(
            "list",
            "/x",
            ftp_response(FtpStatus::RequestFileActionIgnored),
        );
        assert!(matches!(e, BlinkError::Transport(_)), "{e:?}");
    }

    #[test]
    fn sftp_op_and_path_appear_in_message() {
        let e = map_sftp(
            "open",
            "/etc/secret",
            sftp_status(StatusCode::PermissionDenied),
        );
        let msg = format!("{e}");
        assert!(msg.contains("open"), "{msg}");
        assert!(msg.contains("/etc/secret"), "{msg}");
    }

    // -- audit fixes ---------------------------------------------------------

    fn ftp_550(body: &str) -> BlinkError {
        map_ftp(
            "list",
            "/x",
            FtpError::UnexpectedResponse(Response {
                status: FtpStatus::FileUnavailable,
                body: format!("550 {body}").into_bytes(),
            }),
        )
    }

    /// Every 550 was "not found", so a folder blink couldn't list read as
    /// empty.
    #[test]
    fn a_550_is_classified_by_what_the_server_says() {
        for denied in [
            "Permission denied",
            "Access is denied.",
            "Operation not permitted",
            "Not allowed to list",
        ] {
            assert!(
                matches!(ftp_550(denied), BlinkError::Permission(_)),
                "{denied}"
            );
        }
        for missing in [
            "No such file or directory",
            "File not found",
            "The system cannot find the file specified.",
            "Can't access /x: No such file or directory",
            "Failed to change directory.",
            "",
        ] {
            assert!(
                matches!(ftp_550(missing), BlinkError::NotFound(_)),
                "{missing}"
            );
        }
    }

    /// `not found: stor /x` dropped the server's reason, which for a 550
    /// is often the only way to tell what went wrong.
    #[test]
    fn messages_keep_the_servers_words() {
        let e = ftp_550("Disk quota exceeded").to_string();
        assert!(
            e.contains("Disk quota exceeded") && e.contains("list /x"),
            "{e}"
        );
        let e = map_sftp("open", "/x", sftp_status(StatusCode::NoSuchFile)).to_string();
        assert!(e.contains("open /x") && e.contains("test"), "{e}");
        let e = map_sftp("open", "/x", sftp_status(StatusCode::PermissionDenied)).to_string();
        assert!(e.contains("test"), "{e}");
    }

    /// A lost data connection failed the job; as a dropped connection it
    /// gets the dispatcher's one retry.
    #[test]
    fn a_lost_data_connection_is_a_dropped_connection() {
        for status in [
            FtpStatus::CannotOpenDataConnection,
            FtpStatus::TransferAborted,
        ] {
            let e = map_ftp("retr", "/x", ftp_response(status));
            assert!(matches!(e, BlinkError::Disconnected(_)), "{e:?}");
        }
    }

    /// The channel is out of step; carrying on with it means every later
    /// request is suspect.
    #[test]
    fn sftp_protocol_confusion_is_a_dropped_connection() {
        for err in [
            SftpError::UnexpectedPacket,
            SftpError::UnexpectedBehavior("odd".into()),
        ] {
            assert!(matches!(
                map_sftp("read", "/x", err),
                BlinkError::Disconnected(_)
            ));
        }
        assert!(matches!(
            map_sftp("write", "/x", SftpError::Limited("too big".into())),
            BlinkError::Transport(_)
        ));
    }
}
