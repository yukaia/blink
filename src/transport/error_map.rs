//! Map transport-library errors to the typed `BlinkError` surface.
//!
//! Previously every SFTP / FTP call site did
//! `BlinkError::transport(format!("{op} {path}: {err}"))`, which
//! collapsed every failure — file-not-found, permission-denied,
//! connection-dropped, protocol-violation — into a single string-only
//! variant. Callers couldn't distinguish "the file is gone" from
//! "the server hung up mid-`read_dir`", so e.g. recursive walks
//! marched on against half a tree instead of bailing.
//!
//! [`map_sftp`] and [`map_ftp`] do the classification once, in one
//! place, so call sites just thread the `op` and `path` context and
//! get back the right `BlinkError` variant.
//!
//! Status codes that map outside the three new typed variants
//! (NotFound / Permission / Disconnected) fall through to the
//! existing `Transport(...)` string — those are the genuinely
//! unclassifiable failures.

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
/// - Everything else → [`BlinkError::Transport`] with the underlying
///   message preserved.
pub fn map_sftp(op: &str, path: &str, err: SftpError) -> BlinkError {
    match &err {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => BlinkError::not_found(format!("{op} {path}")),
            StatusCode::PermissionDenied => {
                BlinkError::permission(format!("{op} {path}"))
            }
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                BlinkError::disconnected(format!("{op} {path}: {err}"))
            }
            _ => BlinkError::transport(format!("{op} {path}: {err}")),
        },
        SftpError::IO(_) | SftpError::Timeout => {
            BlinkError::disconnected(format!("{op} {path}: {err}"))
        }
        _ => BlinkError::transport(format!("{op} {path}: {err}")),
    }
}

/// Classify a `suppaftp` error against `(op, path)` context.
///
/// - `ConnectionError(_)` / `SecureError(_)` → [`BlinkError::Disconnected`]
/// - `UnexpectedResponse(550 FileUnavailable)` → [`BlinkError::NotFound`].
///   FTP overloads 550 for "file not found" and "no access" — the
///   former is the dominant interpretation across server
///   implementations, and the message body that distinguishes them is
///   server-specific.
/// - `UnexpectedResponse(530 NotLoggedIn)` → [`BlinkError::AuthFailed`]
///   so callers see the right state (vs. a generic transport error).
/// - Everything else → [`BlinkError::Transport`].
pub fn map_ftp(op: &str, path: &str, err: FtpError) -> BlinkError {
    match &err {
        FtpError::ConnectionError(_) | FtpError::SecureError(_) => {
            BlinkError::disconnected(format!("{op} {path}: {err}"))
        }
        FtpError::UnexpectedResponse(r) => match r.status {
            FtpStatus::FileUnavailable => BlinkError::not_found(format!("{op} {path}")),
            FtpStatus::NotLoggedIn => {
                BlinkError::auth(format!("{op} {path}: {err}"))
            }
            _ => BlinkError::transport(format!("{op} {path}: {err}")),
        },
        _ => BlinkError::transport(format!("{op} {path}: {err}")),
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
    fn ftp_bad_response_falls_back_to_transport() {
        let e = map_ftp("list", "/x", FtpError::BadResponse);
        assert!(matches!(e, BlinkError::Transport(_)), "{e:?}");
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
        let e = map_sftp("open", "/etc/secret", sftp_status(StatusCode::PermissionDenied));
        let msg = format!("{e}");
        assert!(msg.contains("open"), "{msg}");
        assert!(msg.contains("/etc/secret"), "{msg}");
    }
}
