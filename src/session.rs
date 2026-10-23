use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle as TaskHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::error::Error;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PONG_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
}

impl Keepalive {
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::parse(
            std::env::var("SELECTIVE_WS_PING_INTERVAL_MS")
                .ok()
                .as_deref(),
            std::env::var("SELECTIVE_WS_PONG_TIMEOUT_MS")
                .ok()
                .as_deref(),
        )
    }

    pub(crate) fn parse(interval_ms: Option<&str>, pong_timeout_ms: Option<&str>) -> Option<Self> {
        let ping_interval = parse_ms(interval_ms).filter(|interval| !interval.is_zero())?;
        Some(Self {
            ping_interval,
            pong_timeout: parse_ms(pong_timeout_ms).unwrap_or(DEFAULT_PONG_TIMEOUT),
        })
    }

    #[must_use]
    pub fn budget(&self) -> Duration {
        self.ping_interval + self.pong_timeout
    }
}

fn parse_ms(value: Option<&str>) -> Option<Duration> {
    value?.parse::<u64>().ok().map(Duration::from_millis)
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
    /// # Errors
    /// Fails when the handshake fails or takes longer than 10 s.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        Self::connect_with(url, Keepalive::from_env()).await
    }

    /// # Errors
    /// Fails when the handshake fails or takes longer than 10 s.
    pub async fn connect_with(url: &str, keepalive: Option<Keepalive>) -> Result<Self, Error> {
        let (socket, _) = tokio::time::timeout(IO_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| Error::new("timed out connecting"))??;
        let (sink, stream) = socket.split();
        let (out_tx, out_rx) = unbounded_channel();
        let (in_tx, in_rx) = unbounded_channel();
        let heard = Arc::new(Mutex::new(Instant::now()));
        let writer = tokio::spawn(write_loop(
            sink,
            out_rx,
            in_tx.clone(),
            Arc::clone(&heard),
            keepalive,
        ));
        let reader = tokio::spawn(read_loop(stream, in_tx, heard));

        Ok(Self {
            outgoing: out_tx,
            incoming: in_rx,
            tasks: vec![writer, reader],
        })
    }

    /// # Errors
    /// Fails when the connection errors. `Ok(None)` means it closed.
    pub async fn recv(&mut self) -> Result<Option<String>, Error> {
        recv_incoming(&mut self.incoming).await
    }

    pub(crate) fn poll(&mut self) -> Result<PollFrame, Error> {
        poll_frames(&mut self.incoming)
    }

    /// # Errors
    /// Fails when the session has ended or the frame can't be written.
    pub async fn send(&self, frame: &str) -> Result<(), Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.outgoing
            .send(OutMsg::Text(frame.to_owned(), reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        reply_rx.await.map_err(|_| Error::new("session closed"))?
    }

    /// # Errors
    /// Fails when the session has ended or the close frame can't be written.
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

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Frames = UnboundedSender<Result<Option<String>, Error>>;

async fn write_loop(
    mut sink: SplitSink<Socket, Message>,
    mut out_rx: UnboundedReceiver<OutMsg>,
    writer_in: Frames,
    heard: Arc<Mutex<Instant>>,
    keepalive: Option<Keepalive>,
) {
    let mut next_ping = keepalive.map(|keepalive| Instant::now() + keepalive.ping_interval);
    loop {
        let deadline = keepalive.map(|keepalive| *heard.lock().unwrap() + keepalive.budget());
        tokio::select! {
            biased;
            msg = out_rx.recv() => {
                match msg {
                    Some(OutMsg::Text(text, reply)) => {
                        let result = send_while_alive(&mut sink, Message::Text(text), &heard, keepalive).await;
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
                            Ok(Ok(()) | Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed
                                | tokio_tungstenite::tungstenite::Error::ConnectionClosed)) => Ok(()),
                            Ok(Err(err)) => Err(Error::from(err)),
                            Err(_) => Err(Error::new("timed out closing the session")),
                        };
                        let _ = reply.send(result);
                        break;
                    }
                    None => break,
                }
            }
            () = sleep_until(deadline) => {
                if let Some(keepalive) = keepalive.filter(|keepalive| silent_for(&heard, keepalive.budget())) {
                    eprintln!("{}", keepalive_timeout(keepalive));
                    let _ = writer_in.send(Ok(None));
                    break;
                }
            }
            () = sleep_until(next_ping) => {
                next_ping = keepalive.map(|keepalive| Instant::now() + keepalive.ping_interval);
                if let Err(err) = send_while_alive(&mut sink, Message::Ping(b"selective".to_vec()), &heard, keepalive).await {
                    eprintln!("{err}");
                    let _ = writer_in.send(Ok(None));
                    break;
                }
            }
        }
    }
}

async fn send_while_alive(
    sink: &mut SplitSink<Socket, Message>,
    message: Message,
    heard: &Mutex<Instant>,
    keepalive: Option<Keepalive>,
) -> Result<(), Error> {
    let sending = sink.send(message);
    let Some(keepalive) = keepalive else {
        return sending.await.map_err(Error::from);
    };
    tokio::pin!(sending);
    loop {
        let deadline = *heard.lock().unwrap() + keepalive.budget();
        tokio::select! {
            sent = &mut sending => return sent.map_err(Error::from),
            () = tokio::time::sleep_until(deadline) => {
                if silent_for(heard, keepalive.budget()) {
                    return Err(Error::new(keepalive_timeout(keepalive)));
                }
            }
        }
    }
}

fn silent_for(heard: &Mutex<Instant>, budget: Duration) -> bool {
    heard.lock().unwrap().elapsed() >= budget
}

fn keepalive_timeout(keepalive: Keepalive) -> String {
    format!(
        "websocket keepalive timeout: no frame from the server for {}s",
        keepalive.budget().as_secs_f64()
    )
}

async fn read_loop(mut stream: SplitStream<Socket>, in_tx: Frames, heard: Arc<Mutex<Instant>>) {
    loop {
        let next = stream.next().await;
        if let Some(Ok(_)) = next {
            *heard.lock().unwrap() = Instant::now();
        }
        match next {
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
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
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
}

#[derive(Debug)]
pub enum PollFrame {
    Empty,
    Frame(String),
    Closed,
}

pub(crate) async fn recv_incoming(
    incoming: &mut UnboundedReceiver<Result<Option<String>, Error>>,
) -> Result<Option<String>, Error> {
    incoming.recv().await.unwrap_or(Ok(None))
}

pub(crate) fn poll_frames(
    incoming: &mut UnboundedReceiver<Result<Option<String>, Error>>,
) -> Result<PollFrame, Error> {
    match incoming.try_recv() {
        Ok(Ok(Some(frame))) => Ok(PollFrame::Frame(frame)),
        Ok(Ok(None)) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            Ok(PollFrame::Closed)
        }
        Ok(Err(err)) => Err(err),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(PollFrame::Empty),
    }
}

enum BlockingCmd {
    Send(String, Sender<Result<(), Error>>),
    Close(Sender<Result<(), Error>>),
}

pub struct BlockingSession {
    commands: UnboundedSender<BlockingCmd>,
    events: std::sync::Mutex<Receiver<Result<Option<String>, Error>>>,
}

impl BlockingSession {
    /// # Errors
    /// Fails when the runtime can't start or the handshake fails or takes longer than 10 s.
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
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                loop {
                    tokio::select! {
                        biased;
                        command = cmd_rx.recv() => {
                            match command {
                                Some(BlockingCmd::Send(frame, reply)) => {
                                    let _ = reply.send(session.send(&frame).await);
                                }
                                Some(BlockingCmd::Close(reply)) => {
                                    let _ = reply.send(session.close().await);
                                    break;
                                }
                                None => break,
                            }
                        }
                        incoming = session.recv() => {
                            match incoming {
                                Ok(frame) => {
                                    let closed = frame.is_none();
                                    if evt_tx.send(Ok(frame)).is_err() || closed {
                                        break;
                                    }
                                }
                                Err(err) => {
                                    let _ = evt_tx.send(Err(err));
                                    break;
                                }
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

    /// # Errors
    /// Fails when no frame arrives within `timeout` or the connection errors.
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

    /// # Errors
    /// Fails when the session has ended or the frame can't be written.
    pub fn send(&self, frame: &str) -> Result<(), Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(BlockingCmd::Send(frame.to_owned(), reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        wait_reply(&reply_rx)
    }

    /// # Errors
    /// Fails when the session has ended or the close frame can't be written.
    pub fn close(&self) -> Result<(), Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(BlockingCmd::Close(reply_tx))
            .map_err(|_| Error::new("session closed"))?;
        wait_reply(&reply_rx)
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn wait_reply(reply_rx: &Receiver<Result<(), Error>>) -> Result<(), Error> {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_is_off_unless_the_client_sets_an_interval() {
        assert_eq!(Keepalive::parse(None, Some("1000")), None);
        assert_eq!(Keepalive::parse(Some("0"), None), None);
        assert_eq!(
            Keepalive::parse(Some("250"), None),
            Some(Keepalive {
                ping_interval: Duration::from_millis(250),
                pong_timeout: DEFAULT_PONG_TIMEOUT,
            })
        );
        assert_eq!(
            Keepalive::parse(Some("250"), Some("0")).map(|keepalive| keepalive.pong_timeout),
            Some(Duration::ZERO)
        );
        assert_eq!(
            Keepalive::parse(Some("20000"), None).map(|keepalive| keepalive.budget()),
            Some(Duration::from_secs(65))
        );
    }
}
