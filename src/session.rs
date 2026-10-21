use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle as TaskHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::error::Error;

const IO_TIMEOUT: Duration = Duration::from_secs(10);

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
        let (socket, _) = tokio::time::timeout(IO_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| Error::new("timed out connecting"))??;
        let (sink, mut stream) = socket.split();
        let (out_tx, mut out_rx) = unbounded_channel();
        let (in_tx, in_rx) = unbounded_channel();
        let writer = tokio::spawn(async move {
            let mut sink = sink;
            while let Some(msg) = out_rx.recv().await {
                match msg {
                    OutMsg::Text(text, reply) => {
                        let result =
                            match tokio::time::timeout(IO_TIMEOUT, sink.send(Message::Text(text)))
                                .await
                            {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(err)) => Err(Error::from(err)),
                                Err(_) => Err(Error::new("timed out sending a frame")),
                            };
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        if failed {
                            break;
                        }
                    }
                    OutMsg::Close(reply) => {
                        let result =
                            match tokio::time::timeout(IO_TIMEOUT, sink.send(Message::Close(None)))
                                .await
                            {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed))
                                | Ok(Err(
                                    tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                                )) => Ok(()),
                                Ok(Err(err)) => Err(Error::from(err)),
                                Err(_) => Err(Error::new("timed out closing the session")),
                            };
                        let _ = reply.send(result);
                        break;
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
