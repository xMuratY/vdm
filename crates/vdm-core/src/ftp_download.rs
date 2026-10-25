//! FTP downloads: one connection, resumed with REST from the end of the part.

use std::io::SeekFrom;
use std::time::Duration;

use reqwest::Url;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

use crate::auth::login_for;
use crate::download::{Completed, DownloadRequest, Downloader, Observer, Progress, Started};
use crate::error::{Error, IoContext, Result};
use crate::filename;
use crate::ftp::{self, FtpDownload, FtpProbe};
use crate::limiter::acquire_all;
use crate::part::{PartMeta, Slot};
use crate::probe::ProbeInfo;

const CHUNK: usize = 64 * 1024;

pub(crate) fn is_ftp(url: &Url) -> bool {
    matches!(url.scheme(), "ftp" | "ftps")
}

impl Downloader {
    fn ftp_login(&self, url: &Url) -> Option<(String, String)> {
        let login = login_for(&self.options().logins, url.host_str()?)?;
        Some((login.username.clone(), login.password.clone()))
    }

    fn ftp_timeout(&self) -> Duration {
        self.options().connect_timeout.max(self.options().read_timeout)
    }

    /// SIZE / REST / name for an `ftp://` URL, with retries.
    pub(crate) async fn probe_ftp(
        &self,
        url: &Url,
        cancel: &CancellationToken,
    ) -> Result<FtpProbe> {
        let login = self.ftp_login(url);
        let login = login.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
        let mut failures = 0;
        loop {
            let attempt = tokio::select! {
                r = ftp::probe(url, login, self.ftp_timeout()) => r,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
            };
            match attempt {
                Ok(probe) => return Ok(probe),
                Err(err) => {
                    failures += 1;
                    self.backoff(err, failures, cancel, 0).await?;
                }
            }
        }
    }

    pub(crate) async fn probe_ftp_info(
        &self,
        url: &Url,
        cancel: &CancellationToken,
    ) -> Result<ProbeInfo> {
        let probe = self.probe_ftp(url, cancel).await?;
        Ok(ProbeInfo {
            final_url: url.to_string(),
            size: probe.size,
            resumable: probe.resumable && probe.size.is_some(),
            etag: None,
            last_modified: None,
            content_type: None,
            file_name: probe.file_name,
        })
    }

    pub(crate) async fn download_ftp<O: Observer>(
        &self,
        req: &DownloadRequest,
        url: &Url,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed> {
        let probe = self.probe_ftp(url, cancel).await?;
        let resumable = probe.resumable && probe.size.is_some();
        let base = match &req.file_name {
            Some(name) => filename::sanitize(name),
            None => probe.file_name.clone(),
        };
        tokio::fs::create_dir_all(&req.out_dir).await.at(&req.out_dir)?;
        let (slot, _claim) = Slot::claim(&req.out_dir, &base, url.as_str(), self.claims()).await?;
        let current = PartMeta::new(url.as_str(), probe.size, None, None);

        // Resume only the same file (same size) from a server that supports REST.
        let on_disk = tokio::fs::metadata(&slot.part).await.map(|m| m.len()).ok();
        let resume = resumable
            && slot.previous.as_ref().is_some_and(|prev| !prev.differs(&current))
            && on_disk.is_some_and(|len| probe.size.is_some_and(|size| len <= size));
        let restarted = slot.previous.is_some() && !resume;
        if !resume {
            slot.write_meta(&current).await?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!resume)
            .open(&slot.part)
            .await
            .at(&slot.part)?;
        let mut offset = if resume { on_disk.unwrap_or(0) } else { 0 };
        let mut writer = BufWriter::with_capacity(self.options().write_buffer.max(1), file);
        writer.seek(SeekFrom::Start(offset)).await.at(&slot.part)?;
        let resumed_from = offset;
        observer.started(&Started {
            file_name: slot.name(),
            part_path: slot.part.clone(),
            size: probe.size,
            resumable,
            connections: 1,
            resumed_from,
        });

        let login = self.ftp_login(url);
        let login = login.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
        let (mut best, mut failures) = (offset, 0);
        loop {
            if probe.size == Some(offset) {
                break;
            }
            match self
                .ftp_attempt(
                    url,
                    login,
                    &mut offset,
                    probe.size,
                    (&mut writer, &slot.part),
                    req,
                    observer,
                    cancel,
                )
                .await
            {
                Ok(()) => break,
                Err(err) => {
                    writer.flush().await.at(&slot.part)?;
                    if matches!(err, Error::Cancelled { .. }) {
                        return Err(Error::Cancelled { downloaded: offset });
                    }
                    if !resumable {
                        // No REST: the next attempt starts over.
                        writer.get_mut().set_len(0).await.at(&slot.part)?;
                        writer.seek(SeekFrom::Start(0)).await.at(&slot.part)?;
                        offset = 0;
                    }
                    if offset > best {
                        best = offset;
                        failures = 0;
                    }
                    failures += 1;
                    self.backoff(err, failures, cancel, offset).await?;
                }
            }
        }

        writer.flush().await.at(&slot.part)?;
        let file = writer.into_inner();
        file.sync_all().await.at(&slot.part)?;
        drop(file);
        let path = slot.finish().await?;
        tracing::info!(path = %path.display(), size = offset, "FTP download complete");
        observer.progress(Progress { downloaded: offset, total: Some(offset) });
        Ok(Completed { path, size: offset, resumed_from, restarted })
    }

    /// One connection: RETR from `offset` to the end.
    #[allow(clippy::too_many_arguments)]
    async fn ftp_attempt<O: Observer>(
        &self,
        url: &Url,
        login: ftp::Login<'_>,
        offset: &mut u64,
        size: Option<u64>,
        (writer, part): (&mut BufWriter<tokio::fs::File>, &std::path::Path),
        req: &DownloadRequest,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut transfer = tokio::select! {
            r = FtpDownload::open(url, login, *offset, self.ftp_timeout()) => r?,
            () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: *offset }),
        };
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = tokio::select! {
                r = transfer.read(&mut buf) => r?,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: *offset }),
            };
            if n == 0 {
                break;
            }
            if size.is_some_and(|s| *offset + n as u64 > s) {
                return Err(Error::Protocol(
                    "the FTP server sent more data than the file size".into(),
                ));
            }
            if !req.limits.is_empty() {
                tokio::select! {
                    () = acquire_all(&req.limits, n as u64) => {}
                    () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: *offset }),
                }
            }
            writer.write_all(&buf[..n]).await.at(part)?;
            *offset += n as u64;
            observer.progress(Progress { downloaded: *offset, total: size });
        }
        // A missing file or a failed transfer is only reported here.
        transfer.finish().await?;
        match size {
            Some(total) if *offset < total => {
                Err(Error::Incomplete { received: *offset, expected: total })
            }
            _ => Ok(()),
        }
    }
}
