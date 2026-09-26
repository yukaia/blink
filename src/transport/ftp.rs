//! FTP transport via `suppaftp::AsyncFtpStream` (tokio backend).

use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use zeroize::Zeroizing;

use crate::error::{BlinkError, Result};
use crate::session::{AuthMethod, Session};

use super::ftp_impl;

pub struct FtpTransport {
    stream: AsyncFtpStream,
    /// What [`Self::reopen`] connects with. See `delegate_ftp_transport!`.
    session: Session,
    password: Option<Zeroizing<String>>,
    /// Set while a call is in flight and after one ends in `Disconnected`;
    /// the next call reconnects first. See `delegate_ftp_transport!`.
    broken: bool,
}

impl FtpTransport {
    pub async fn connect(session: &Session, password: Option<&str>) -> Result<Self> {
        Ok(Self {
            stream: Self::open(session, password).await?,
            session: session.clone(),
            password: password.map(|p| Zeroizing::new(p.to_string())),
            broken: false,
        })
    }

    /// Replace the stream with a fresh connection and login.
    async fn reopen(&mut self) -> Result<()> {
        let password = self.password.as_ref().map(|p| p.as_str());
        self.stream = Self::open(&self.session, password).await?;
        Ok(())
    }

    async fn open(session: &Session, password: Option<&str>) -> Result<AsyncFtpStream> {
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

        Ok(stream)
    }
}

ftp_impl::delegate_ftp_transport!(FtpTransport, Ftp);
