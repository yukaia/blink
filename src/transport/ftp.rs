//! FTP transport via `suppaftp::AsyncFtpStream` (tokio backend).
//!
//! Connecting and logging in are shared with FTPS: see
//! [`ftp_impl::open_control`] and [`ftp_impl::log_in`].

use suppaftp::tokio::AsyncFtpStream;
use zeroize::Zeroizing;

use crate::error::Result;
use crate::session::Session;

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
        let mut stream: AsyncFtpStream = ftp_impl::open_control(session, "ftp").await?;
        ftp_impl::log_in(&mut stream, session, password, "ftp").await?;
        Ok(stream)
    }
}

ftp_impl::delegate_ftp_transport!(FtpTransport, Ftp);
