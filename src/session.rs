use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle as TaskHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::error::Error;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);
pub const DEFAULT_PONG_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
}

impl Keepalive {
    pub fn from_env() -> Self {
        Self::parse(
            std::env::var("SELECTIVE_WS_PING_INTERVAL_MS")
                .ok()
                .as_deref(),
            std::env::var("SELECTIVE_WS_PONG_TIMEOUT_MS")
                .ok()
                .as_deref(),
        )
    }

    pub fn parse(interval_ms: Option<&str>, pong_timeout_ms: Option<&str>) -> Self {
        let ping_interval = parse_ms(interval_ms).unwrap_or(DEFAULT_PING_INTERVAL);
        let mut pong_timeout = parse_ms(pong_timeout_ms).unwrap_or(DEFAULT_PONG_TIMEOUT);
        if ping_interval.is_zero() {
            return Self {
                ping_interval,
                pong_timeout: Duration::ZERO,
            };
        }
        if pong_timeout.is_zero() {
            pong_timeout = ping_interval;
        }
        Self {
            ping_interval,
            pong_timeout,
        }
    }
}

fn parse_ms(value: Option<&str>) -> Option<Duration> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    value.parse::<u64>().ok().map(Duration::from_millis)
}

enum OutMsg {
    Text(String, oneshot::Sender<Result<(), Error>>),
    Close(oneshot::Sender<Result<(), Error>>),
}

pub struct Session {
    outgoing: UnboundedSender<OutMsg>,
    incoming: UnboundedReceiver<Result<Option<String>, Error>>,
    tasks: Vec<TaskHandle<()>>,
}

impl Session {
    pub async fn connect(url: &str) -> Result<Self, Error> {
        Self::connect_with(url, Keepalive::from_env()).await
    }

    pub async fn connect_with(url: &str, keepalive: Keepalive) -> Result<Self, Error> {
        let (socket, _) = tokio::time::timeout(IO_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| Error::new("timed out connecting"))??;
        let (sink, mut stream) = socket.split();
        let (out_tx, mut out_rx) = unbounded_channel();
        let (in_tx, in_rx) = unbounded_channel();
        let writer_in = in_tx.clone();
        let pongs = Arc::new(AtomicU64::new(0));
        let pong_notify = Arc::new(Notify::new());
        let reader_pongs = Arc::clone(&pongs);
        let reader_notify = Arc::clone(&pong_notify);
        let writer = tokio::spawn(async move {
            let mut sink = sink;
            let mut next_ping = tokio::time::Instant::now() + keepalive.ping_interval;
            let mut outstanding: Option<(u64, tokio::time::Instant)> = None;
            loop {
                if let Some((stamp, _)) = outstanding {
                    if pongs.load(Ordering::SeqCst) != stamp {
                        outstanding = None;
                    }
                }
                let scheduled_ping = (outstanding.is_none() && !keepalive.ping_interval.is_zero())
                    .then_some(next_ping);
                let pong_deadline = outstanding.map(|(_, deadline)| deadline);
                let notified = pong_notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    biased;
                    msg = out_rx.recv() => {
                        match msg {
                            Some(OutMsg::Text(text, reply)) => {
                                let result = match tokio::time::timeout(
                                    IO_TIMEOUT,
                                    sink.send(Message::Text(text)),
                                )
                                .await
                                {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(err)) => Err(Error::from(err)),
                                    Err(_) => Err(Error::new("timed out sending a frame")),
                                };
                                let failed = result.is_err();
                                let _ = reply.send(result);
                                if failed {
                                    let _ = writer_in.send(Ok(None));
                                    break;
                                }
                            }
                            Some(OutMsg::Close(reply)) => {
                                let result = match tokio::time::timeout(
                                    IO_TIMEOUT,
                                    sink.send(Message::Close(None)),
                                )
                                .await
                                {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(
                                        tokio_tungstenite::tungstenite::Error::AlreadyClosed
                                        | tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                                    )) => Ok(()),
                                    Ok(Err(err)) => Err(Error::from(err)),
                                    Err(_) => Err(Error::new("timed out closing the session")),
                                };
                                let _ = reply.send(result);
                                break;
                            }
                            None => break,
                        }
                    }
                    _ = notified, if outstanding.is_some() => {
                        outstanding = None;
                    }
                    _ = sleep_until(pong_deadline), if pong_deadline.is_some() => {
                        let timed_out = outstanding
                            .is_some_and(|(stamp, _)| pongs.load(Ordering::SeqCst) == stamp);
                        outstanding = None;
                        if timed_out {
                            eprintln!("websocket pong timeout");
                            let _ = sink.send(Message::Close(None)).await;
                            let _ = writer_in.send(Ok(None));
                            break;
                        }
                    }
                    _ = sleep_until(scheduled_ping), if scheduled_ping.is_some() => {
                        let stamp = pongs.load(Ordering::SeqCst);
                        if sink.send(Message::Ping(b"selective".to_vec())).await.is_err() {
                            eprintln!("websocket ping failed");
                            let _ = writer_in.send(Ok(None));
                            break;
                        }
                        outstanding = Some((
                            stamp,
                            tokio::time::Instant::now() + keepalive.pong_timeout,
                        ));
                        next_ping = tokio::time::Instant::now() + keepalive.ping_interval;
                    }
                }
            }
        });

        let reader = tokio::spawn(async move {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if in_tx.send(Ok(Some(text))).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        if in_tx.send(Ok(Some(text))).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        reader_pongs.fetch_add(1, Ordering::SeqCst);
                        reader_notify.notify_one();
                    }
                    Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = in_tx.send(Ok(None));
                        break;
                    }
                    Some(Err(err)) => {
                        let _ = in_tx.send(Err(Error::from(err)));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            outgoing: out_tx,
            incoming: in_rx,
            tasks: vec![writer, reader],
        })
    }

    pub async fn recv(&mut self) -> Result<Option<String>, Error> {
        match self.incoming.recv().await {
            Some(result) => result,
            None => Ok(None),
        }
    }

    pub fn poll(&mut self) -> Result<PollFrame, Error> {
        match self.incoming.try_recv() {
            Ok(Ok(Some(frame))) => Ok(PollFrame::Frame(frame)),
            Ok(Ok(None)) => Ok(PollFrame::Closed),
            Ok(Err(err)) => Err(err),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(PollFrame::Empty),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Ok(PollFrame::Closed),
        }
    }

    pub async fn send(&self, frame: &str) -> Result<(), Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.outgoing
            .send(OutMsg::Text(frame.to_owned(), reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        reply_rx.await.map_err(|_| Error::new("session closed"))?
    }

    pub async fn close(&self) -> Result<(), Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.outgoing
            .send(OutMsg::Close(reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        reply_rx.await.map_err(|_| Error::new("session closed"))?
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub enum PollFrame {
    Empty,
    Frame(String),
    Closed,
}

enum BlockingCmd {
    Send(String, Sender<Result<(), Error>>),
    Close(Sender<Result<(), Error>>),
}

pub struct BlockingSession {
    commands: UnboundedSender<BlockingCmd>,
    events: std::sync::Mutex<Receiver<Result<Option<String>, Error>>>,
    thread: Option<JoinHandle<()>>,
}

impl BlockingSession {
    pub fn connect(url: &str) -> Result<Self, Error> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cmd_tx, mut cmd_rx) = unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let url = url.to_owned();
        let thread = thread::spawn(move || {
            let runtime = match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(Error::from(err)));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut session = match Session::connect(&url).await {
                    Ok(session) => session,
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                let outgoing = session.outgoing.clone();
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                loop {
                    tokio::select! {
                        incoming = session.incoming.recv() => {
                            match incoming {
                                Some(Ok(frame)) => {
                                    if evt_tx.send(Ok(frame)).is_err() {
                                        break;
                                    }
                                }
                                Some(Err(err)) => {
                                    let _ = evt_tx.send(Err(err));
                                    break;
                                }
                                None => {
                                    let _ = evt_tx.send(Ok(None));
                                    break;
                                }
                            }
                        }
                        command = cmd_rx.recv() => {
                            match command {
                                Some(BlockingCmd::Send(frame, reply)) => {
                                    let (done_tx, done_rx) = oneshot::channel();
                                    let queued = outgoing
                                        .send(OutMsg::Text(frame, done_tx))
                                        .map_err(|_| Error::new("session closed"));
                                    let result = match queued {
                                        Ok(()) => done_rx.await.unwrap_or_else(|_| Err(Error::new("session closed"))),
                                        Err(err) => Err(err),
                                    };
                                    let _ = reply.send(result);
                                }
                                Some(BlockingCmd::Close(reply)) => {
                                    let (done_tx, done_rx) = oneshot::channel();
                                    let queued = outgoing
                                        .send(OutMsg::Close(done_tx))
                                        .map_err(|_| Error::new("session closed"));
                                    let result = match queued {
                                        Ok(()) => done_rx.await.unwrap_or_else(|_| Err(Error::new("session closed"))),
                                        Err(err) => Err(err),
                                    };
                                    let _ = reply.send(result);
                                    break;
                                }
                                None => break,
                            }
                        }
                    }
                }
            });
        });
        match ready_rx.recv_timeout(IO_TIMEOUT + Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                commands: cmd_tx,
                events: std::sync::Mutex::new(evt_rx),
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(thread);
                Err(Error::new("timed out connecting"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(Error::new("session thread stopped before connect"))
            }
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<String>, Error> {
        let events = self
            .events
            .lock()
            .map_err(|_| Error::new("session lock poisoned"))?;
        match events.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Error::new("timed out waiting for a frame"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    pub fn send(&self, frame: &str) -> Result<(), Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(BlockingCmd::Send(frame.to_owned(), reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        wait_reply(reply_rx)
    }

    pub fn close(&self) -> Result<(), Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(BlockingCmd::Close(reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        wait_reply(reply_rx)
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn wait_reply(reply_rx: Receiver<Result<(), Error>>) -> Result<(), Error> {
    match reply_rx.recv_timeout(IO_TIMEOUT + Duration::from_secs(2)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(Error::new("timed out waiting for the session"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::new("session closed")),
    }
}

impl Drop for BlockingSession {
    fn drop(&mut self) {
        let _ = self.close();
        drop(self.thread.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_defaults_and_zero_interval_disables_pings() {
        assert_eq!(
            Keepalive::parse(None, None),
            Keepalive {
                ping_interval: DEFAULT_PING_INTERVAL,
                pong_timeout: DEFAULT_PONG_TIMEOUT,
            }
        );
        assert_eq!(DEFAULT_PING_INTERVAL, Duration::from_secs(20));
        assert!(DEFAULT_PING_INTERVAL < Duration::from_secs(60));
        let disabled = Keepalive::parse(Some("0"), Some("10"));
        assert!(disabled.ping_interval.is_zero());
        assert_eq!(
            Keepalive::parse(Some("250"), None).pong_timeout,
            DEFAULT_PONG_TIMEOUT
        );
        assert_eq!(
            Keepalive::parse(Some("250"), Some("0")).pong_timeout,
            Duration::from_millis(250)
        );
    }
}
