//! FTP transport via `suppaftp::AsyncFtpStream` (tokio backend).

use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;

use crate::error::{BlinkError, Result};
use crate::session::{AuthMethod, Session};

use super::ftp_impl;

pub struct FtpTransport {
    stream: AsyncFtpStream,
}

impl FtpTransport {
    pub async fn connect(session: &Session, password: Option<&str>) -> Result<Self> {
        if !matches!(session.auth, AuthMethod::Password) {
            return Err(BlinkError::auth(
                "FTP only supports password (or anonymous) auth",
            ));
        }

        let addr = format!("{}:{}", session.host, session.port);
        let mut stream = AsyncFtpStream::connect(&addr)
            .await
            .map_err(|e| BlinkError::connect(format!("ftp connect to {addr}: {e}")))?;

        let (user, pw) = if session.username.is_empty() {
            ("anonymous", "anonymous@")
        } else {
            let pw = password.unwrap_or("");
            (session.username.as_str(), pw)
        };
        stream
            .login(user, pw)
            .await
            .map_err(|e| BlinkError::auth(format!("ftp login: {e}")))?;

        stream
            .transfer_type(FileType::Binary)
            .await
            .map_err(|e| BlinkError::transport(format!("set binary: {e}")))?;

        Ok(Self { stream })
    }
}

ftp_impl::delegate_ftp_transport!(FtpTransport, Ftp);
