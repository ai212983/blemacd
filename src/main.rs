use std::collections::hash_map::{Entry, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    prelude::*,
    task,
};
use core_bluetooth::uuid::Uuid;
use env_logger::{Builder, Env};
use futures::{
    AsyncBufReadExt, AsyncWriteExt, channel::mpsc, future::{Either, Future, FutureExt}, select, sink::SinkExt,
    stream::Fuse,
    StreamExt,
};
use lazy_static::lazy_static;
use log::*;
use regex::Regex;

use blemacd::handlers::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Sender<T> = mpsc::UnboundedSender<T>;
type Receiver<T> = mpsc::UnboundedReceiver<T>;

#[derive(Debug)]
enum Void {}

const SOCKET_PATH: &str = "/tmp/blemacd.sock";

const COMMAND_STATUS: &str = "status";
const COMMAND_ALL_DEVICES: &str = "all";
const COMMAND_CONNECTED_DEVICES: &str = "connected";


async fn streams_accept_loop<P: AsRef<Path>>(path: P) -> Result<()> {
    // https://docs.rs/async-std/0.99.3/async_std/os/unix/net/struct.UnixListener.html
    let listener = UnixListener::bind(path).await?;
    let mut peer_id: u32 = 0;
    let (broker_sender, broker_receiver) = mpsc::unbounded();

    let broker_handle = task::spawn(broker_loop(broker_receiver));
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        peer_id = peer_id + 1;
        info!("incoming connection #{:?}", peer_id);
        spawn_and_log_error(peer_reader_loop(broker_sender.clone(), stream, peer_id));
    }
    drop(broker_sender);
    broker_handle.await;
    Ok(())
}

async fn peer_reader_loop(mut broker: Sender<PeerEvent>, stream: UnixStream, idx: u32) -> Result<()> {
    let stream = Arc::new(stream);
    let reader = BufReader::new(&*stream);
    let mut lines = AsyncBufReadExt::lines(reader);

    let (shutdown_sender, shutdown_receiver) = mpsc::unbounded::<Void>();
    broker
        .send(PeerEvent::NewPeer {
            idx,
            stream: stream.clone(),
            shutdown: shutdown_receiver,
        })
        .await
        .unwrap();

    let shutdown = Arc::new(shutdown_sender);
    while let Some(line) = lines.next().await {
        broker
            .send(PeerEvent::Command {
                id: line?,
                peer_id: idx,
                shutdown: shutdown.clone(),
            })
            .await
            .unwrap();
    }

    Ok(())
}

struct Session {
    controller: Controller,
    pending_changes: HashMap<Uuid, fn(u8) -> u8>,
    ranges: HashMap<Uuid, (Option<usize>, Option<usize>)>,
    pending_ranges: HashMap<uuid::Uuid, (Option<usize>, Option<usize>)>,
}

impl Session {
    fn new(controller: Controller) -> Self {
        Self {
            controller,
            pending_changes: Default::default(),
            ranges: Default::default(),
            pending_ranges: Default::default(),
        }
    }

    async fn process(&mut self, input: String) -> String {
        let mut results = vec![];
        let input = &mut input.clone();

        loop {
            match self.get_next_command(input, &results) {
                Either::Left(command) => {
                    results.push(self.controller.execute(command).await);
                }
                Either::Right(reply) => {
                    return reply;
                }
            }
        }
    }

    fn get_next_command(&mut self, input: &mut String, results: &Vec<CommandResult>) -> Either<Command, String> {
        if results.len() == 0 {
            if let InputToken::Address(token, _) = consume_token(input) {
                Either::Left(match token.as_str() {
                    COMMAND_STATUS => Command::GetStatus,
                    COMMAND_ALL_DEVICES => Command::ListDevices,
                    COMMAND_CONNECTED_DEVICES => Command::ListConnectedDevices,
                    _ => {
                        info!("request for peripheral: '{:?}'", token);
                        Command::FindPeripheral(token.to_string())
                    }
                })
            } else {
                Either::Right("empty input".to_string())
            }
        } else {
            if let Some(result) = results.last() {
                match result {
                    CommandResult::GetStatus(uptime, devices) => {
                        let mut status = format!("uptime {}", humantime::format_duration(*uptime));
                        if let Some((all, connected)) = devices {
                            status = status + &*format!(", {} devices detected, {} connected", all, connected);
                        }
                        Either::Right(status)
                    }
                    CommandResult::ListDevices(devices) => {
                        let devices = devices.values()
                            .map(|p| p.to_string())
                            .collect::<Vec<String>>();
                        Either::Right(format!(
                            "{} devices total\n{}",
                            devices.len(),
                            devices.join("\n")
                        ))
                    }
                    CommandResult::FindPeripheral(peripheral) =>
                        if let Some(peripheral) = peripheral {
                            Either::Left(Command::ConnectToPeripheral(peripheral.peripheral.clone()))
                        } else {
                            Either::Right("peripheral match not found".to_string())
                        },
                    CommandResult::ConnectToPeripheral(peripheral) => {
                        info!("connected to peripheral {:?}", peripheral);
                        if input.is_empty() {
                            Either::Right(format!("connected to peripheral {:?}, input: {:?}", peripheral, input))
                        } else {
                            if let InputToken::Address(token, _) = consume_token(input) {
                                info!("Connected to peripheral, searching for Service {:?}", input);
                                Either::Left(Command::FindService(peripheral.clone(), token.to_string()))
                            } else {
                                Either::Right("connected to peripheral".to_string())
                            }
                        }
                    }
                    CommandResult::FindService(peripheral, service) => {
                        if let Some(service) = service {
                            if let InputToken::Address(token, range) = consume_token(input) {
                                info!("service found: {:?}, searching for Characteristic: {:?}", service, token);
                                let uuid = uuid::Uuid::new_v4();
                                if let Some(range) = range {
                                    self.pending_ranges.insert(uuid, range);
                                }
                                Either::Left(Command::FindCharacteristic(peripheral.clone(), service.clone(), token.to_string(), uuid))
                            } else {
                                Either::Right(format!("service found: {:?}", service))
                            }
                        } else {
                            Either::Right("service not found".to_string())
                        }
                    }
                    CommandResult::FindCharacteristic(peripheral, characteristic, uuid) => {
                        if let Some(characteristic) = characteristic {
                            info!("characteristic found: {:?}", characteristic);
                            let pending_range = self.pending_ranges.remove(uuid);
                            Either::Left({
                                if let Some(range) = pending_range {
                                    self.ranges.insert(characteristic.id(), range);
                                };
                                match consume_token(input) {
                                    InputToken::Negation => {
                                        self.pending_changes.insert(
                                            characteristic.id(),
                                            |v| if v == 0 { 1 } else { 0 },
                                        );
                                        Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
                                    }
                                    InputToken::Address(token, _) =>
                                        Command::WriteCharacteristic(
                                            peripheral.clone(), characteristic.clone(), vec![token.parse().unwrap()],
                                        ),
                                    _ => Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
                                }
                            })
                        } else {
                            Either::Right("characteristic not found".to_string())
                        }
                    }
                    CommandResult::ReadCharacteristic(peripheral, characteristic, value) => {
                        if let Some(value) = value {
                            let id = &characteristic.id();
                            let range = self.ranges.remove(id);
                            if let Some((_, change)) = self.pending_changes.remove_entry(id) {
                                info!("characteristic read: {:?}, now updating", characteristic);
                                Either::Left(Command::WriteCharacteristic(
                                    peripheral.clone(), characteristic.clone(), vec![change(value[0])]))
                            } else {
                                info!("characteristic read: {:?} (range {:?})", characteristic, range);
                                Either::Right(get_slice(value, range).iter().map(|v| format!("{:02X?}", v)).collect::<Vec<String>>().join(" "))
                            }
                        } else {
                            Either::Right("characteristic read failed".to_string())
                        }
                    }
                    CommandResult::WriteCharacteristic(_, _, result) => {
                        Either::Right(format!("{:?}", result))
                    }
                    _ => Either::Right("unknown result".to_string())
                }
            } else {
                Either::Right("no results".to_string())
            }
        }
    }
}

async fn peer_writer_loop(
    controller: Controller,
    messages: &mut Receiver<String>,
    stream: Arc<UnixStream>,
    shutdown: Receiver<Void>) -> Result<()> {
    let mut stream = &*stream;
    let mut events = FusedStream::new(Box::pin(messages), Box::pin(shutdown.fuse()));

    let mut session = Session::new(controller);

    while let Some(event) = events.next().await {
        if let Some(r) = &event.reply {
            let r = session.process(r.clone()).await;
            let s = if event.shutdown {
                r.to_owned()
            } else {
                r.to_owned() + "\n"
            };
            AsyncWriteExt::write_all(&mut stream, s.as_bytes()).await?;
        }
        if event.shutdown {
            break;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Reply {
    message: String,
    origin: String,
}

impl Reply {
    fn as_oneliner(&self) -> String {
        self.message.clone()
    }
    fn as_reply(&self) -> String {
        format!("[{}]: {}\n", &self.origin, &self.message.clone())
    }
}


// Base logic taken from https://book.async.rs/tutorial/handling_disconnection.html#final-code
async fn broker_loop(events: Receiver<PeerEvent>) {
    let (_handler, controller) = AsyncManager::new();

    let (disconnect_sender, mut disconnect_receiver) =
        mpsc::unbounded::<(u32, Receiver<String>, Arc<UnixStream>)>();
    let mut peers: HashMap<u32, Sender<String>> = Default::default();
    let mut events = events.fuse();

    loop {
        let peer_event = select! {
            peer_event = events.next().fuse() => match peer_event {
                None => break,
                Some(event) => event,
            },
            disconnect = disconnect_receiver.next() => {
                let (idx, mut pending_replies, stream) = disconnect.unwrap();
                assert!(peers.remove(&idx).is_some());

                let mut stream = &*stream;
                while let Ok(Some(reply)) = pending_replies.try_next() {
                    AsyncWriteExt::write_all(&mut stream, reply.as_bytes())
                        .await.ok();
                }
                info!("peer #{} disconnected, currently connected peers: {}", idx, peers.len());
                continue;
            }
        };

        match peer_event {

            // When processing commands, broker_loop job is to execute command and return Reply
            // to be processed by peer_writer_loop.
            // Some commands can be done immediately, like status or peripherals list.
            // They can be done in main broker_loop execution thread.
            // Other commands requires time to be processed, like connect to peripheral,
            // discover service, read from characteristic. They shouldn't block execution thread,
            // so new task::spawn will be created.
            PeerEvent::Command { id, peer_id, shutdown } => {
                if let Some(peer) = peers.get_mut(&peer_id) {
                    info!("sending: '{}' to {}", id, peer_id);
                    peer.send(id.clone()).await.unwrap(); //TODO(df): Do we have to unwrap?
                }
            }
            PeerEvent::NewPeer {
                idx,
                stream,
                shutdown,
            } => {
                info!("peer #{} connected", idx);
                match peers.entry(idx) {
                    Entry::Occupied(..) => (),
                    Entry::Vacant(entry) => {
                        let (client_sender, mut client_receiver) = mpsc::unbounded();
                        entry.insert(client_sender);
                        let mut disconnect_sender = disconnect_sender.clone();

                        let controller = controller.clone();
                        spawn_and_log_error(async move {
                            let res = peer_writer_loop(
                                controller,
                                &mut client_receiver,
                                stream.clone(),
                                shutdown,
                            ).await;

                            // One-liner will terminate peer_writer_loop fast enough
                            // to leave no pending messages but pending commands to be processed.
                            // It is critical to close stream and push pending messages in the same
                            // thread where commands are processed (broker_loop).

                            // passing Arc<Stream> will prevent dropping stream once we're out of current scope

                            disconnect_sender
                                .send((idx, client_receiver, stream.clone()))
                                .await
                                .unwrap();
                            res
                        });
                    }
                }
            }
        }
    }
    info!("dropping peers");
    drop(peers); // 5
    drop(disconnect_sender); // 6
    while let Some((_name, _pending_replies, _stream)) = disconnect_receiver.next().await {}
}

struct FusedStream<'a> {
    messages: Pin<Box<&'a mut Receiver<String>>>,
    shutdown: Pin<Box<Fuse<Receiver<Void>>>>,
}

impl<'a> FusedStream<'a> {
    fn new(
        messages: Pin<Box<&'a mut Receiver<String>>>,
        shutdown: Pin<Box<Fuse<Receiver<Void>>>>,
    ) -> FusedStream {
        Self { messages, shutdown }
    }
}

#[derive(Debug)]
struct FusedEvent {
    reply: Option<String>,
    shutdown: bool,
}

// TODO(df): I am pretty sure there is something like this in async streams?
impl<'a> Stream for FusedStream<'a> {
    type Item = FusedEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let m = Pin::new(&mut self.messages).poll_next(cx);
        let s = Pin::new(&mut self.shutdown).poll_next(cx);

        if m.is_ready() || s.is_ready() {
            Poll::Ready(Some(FusedEvent {
                reply: match m {
                    Poll::Ready(reply) => reply,
                    Poll::Pending => None,
                },
                shutdown: match s {
                    Poll::Ready(None) => true,
                    _ => false,
                },
            }))
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
enum PeerEvent {
    NewPeer {
        idx: u32,
        stream: Arc<UnixStream>,
        shutdown: Receiver<Void>,
    },
    Command {
        id: String,
        peer_id: u32,
        shutdown: Arc<Sender<Void>>,
    },
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
    where
        F: Future<Output=Result<()>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            eprintln!("{}", e)
        }
    })
}

fn cleanup_socket() {
    std::fs::remove_file(SOCKET_PATH)
        .map_err(|err| eprintln!("{:?}", err))
        .ok();
}

pub fn main() {
    let env = Env::default();

    Builder::from_env(env)
        .format_level(false)
        .format_timestamp_micros()
        .init();

    cleanup_socket();

    task::block_on(streams_accept_loop(SOCKET_PATH))
        .map_err(|err| eprintln!("{:?}", err))
        .ok();

    cleanup_socket();
    info!("exiting application");
}

// -------
#[derive(PartialEq, Debug)]
enum InputToken {
    Address(String, Option<(Option<usize>, Option<usize>)>),
    Negation,
    None,
}

fn to_int(m: Option<regex::Match>) -> Option<usize> {
    if let Some(m) = m {
        if !m.range().is_empty() {
            return m.as_str().parse::<usize>().ok();
        }
    }
    None
}

fn consume_token(input: &mut String) -> InputToken {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^(?:/)?([^/\[\s]+)(?:\[(\d*)(?:..(\d*))?\])?(?:/)?").unwrap();
    }

    let cloned = input.clone();
    for cap in RE.captures_iter(cloned.as_str()) {
        if let Some(addr) = cap.get(1) {
            if addr.range().is_empty() {
                continue;
            } else {
                *input = input.get(cap.get(0).unwrap().end()..).unwrap().to_string();
                return match addr.as_str() {
                    "!" => InputToken::Negation,
                    _ => InputToken::Address(addr.as_str().to_string(), {
                        let start = cap.get(2);
                        let end = cap.get(3);
                        if start.is_some() || end.is_some() {
                            Some(if let Some(s) = to_int(start) {
                                (Some(s),
                                 if let Some(e) = end {
                                     e.as_str().parse::<usize>().ok()
                                 } else {
                                     Some(s + 1)
                                 })
                            } else {
                                (None, to_int(end))
                            })
                        } else {
                            None
                        }
                    })
                };
            }
        }
    };

    InputToken::None
}

fn get_slice(vector: &Vec<u8>, slice: Option<(Option<usize>, Option<usize>)>) -> &[u8] {
    match slice {
        None => vector,
        Some(range) => match range {
            (Some(start), Some(end)) => &vector[start..end],
            (Some(start), None) => &vector[start..],
            (None, Some(end)) => &vector[..end],
            (None, None) => &vector[..],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negation_token() {
        let mut input = &mut "!/212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Negation);
        assert_eq!(input, "212[51..75]/32");

        let mut input = &mut "!/".to_string();
        assert_eq!(consume_token(input), InputToken::Negation);
        assert_eq!(input, "");
    }

    #[test]
    fn address_token() {
        let mut input = &mut "/212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let mut input = &mut "212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let mut input = &mut "212/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "32");

        let mut input = &mut "212".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "");
    }

    #[test]
    fn range_token() {
        let mut input = &mut "1[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("1".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let mut input = &mut "/2[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("2".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let mut input = &mut "/3[51..]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("3".to_string(), Some((Some(51), None))));
        assert_eq!(input, "32");

        let mut input = &mut "/4[..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("4".to_string(), Some((None, Some(75)))));
        assert_eq!(input, "32");

        let mut input = &mut "/5[6]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("5".to_string(), Some((Some(6), Some(7)))));
        assert_eq!(input, "32");

        let mut input = &mut "/6[..]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("6".to_string(), Some((None, None))));
        assert_eq!(input, "32");

        let mut input = &mut "/7[]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("7".to_string(), Some((None, None))));
        assert_eq!(input, "32");
    }

    #[test]
    fn consume_source_string() {
        let mut input = &mut "/32/988".to_string();
        assert_eq!(consume_token(input), InputToken::Address("32".to_string(), None));
        assert_eq!(input, "988");

        let mut input = &mut "212/32/988".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "32/988");

        let mut input = &mut "212".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "");
        assert_eq!(consume_token(input), InputToken::None);
    }
}