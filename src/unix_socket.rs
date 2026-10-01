use anyhow::Context;
use log::{error, info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use std::path::Path;
use std::time::Duration;

use crate::notification;

const RECONNECT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct UnixSocketNotifier {
    listener: UnixListener,
    notif_rx: broadcast::Receiver<notification::ChargeState>,
}

impl UnixSocketNotifier {
    pub fn new(notif_rx: broadcast::Receiver<notification::ChargeState>) -> anyhow::Result<Self> {
        let socket_path = Path::new(notification::UNIX_SOCKET_PATH);
        if socket_path.exists() {
            std::fs::remove_file(socket_path).context("unix socket")?;
        }

        let listener = UnixListener::bind(socket_path).context("binding unix socket")?;

        Ok(UnixSocketNotifier { listener, notif_rx })
    }

    pub async fn into_task(mut self, mut stop_rx: broadcast::Receiver<()>) {
        tokio::select! {
            biased;
            _  = stop_rx.recv() => {
                info!("shutting down due to stop request");
            }
            _ = self.accept() => (),
        }

        self.remove_socket();
    }

    pub async fn accept(&mut self) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    if let Err(err) = self.serve(stream).await {
                        info!("error serving unix socket: {err}");
                    }
                }
                Err(err) => {
                    warn!("error accepting unix socket: {err}");
                }
            }

            tokio::time::sleep(RECONNECT).await;
        }
    }

    async fn serve(&mut self, mut stream: UnixStream) -> anyhow::Result<()> {
        let mut buf = vec![0; 1024];
        loop {
            let msg = self.notif_rx.recv().await.context("notif chan receiver")?;

            buf.clear();
            serde_json::to_writer(&mut buf, &msg)?;

            stream
                .write(buf.as_slice())
                .await
                .context("writting to unix socket")?;
        }
    }

    fn remove_socket(&self) {
        let socket_path = Path::new(notification::UNIX_SOCKET_PATH);
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path)
                .inspect_err(|err| error!("error removing socket {err}"));
        }
    }
}

impl Drop for UnixSocketNotifier {
    fn drop(&mut self) {
        self.remove_socket();
    }
}
