use std::collections::hash_map::{Entry, HashMap};
use std::sync::Arc;

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    task,
};
use core_bluetooth::uuid::Uuid;
use env_logger::{Builder, Env};
use futures::{
    AsyncBufReadExt, AsyncWriteExt, channel::mpsc, future::{Either, Future, FutureExt}, select, sink::SinkExt,
    StreamExt,
};
use log::*;

use blemacd::{byte_slices::*, commands::*, handlers::*, input_token::*, shutting_down_stream::*};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Sender<T> = mpsc::UnboundedSender<T>;
type Receiver<T> = mpsc::UnboundedReceiver<T>;

#[derive(Debug)]
enum Void {}

const SOCKET_PATH: &str = "/tmp/blemacd.sock";

const COMMAND_STATUS: &str = "status";
const COMMAND_PERIPHERALS: &str = "peripherals";

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
        .await.unwrap();

    let shutdown = Arc::new(shutdown_sender);
    while let Some(line) = lines.next().await {
        broker
            .send(PeerEvent::Command {
                id: line?,
                peer_id: idx,
                shutdown: shutdown.clone(),
            })
            .await.unwrap();
    }

    Ok(())
}


struct Session {
    controller: Controller,
    pending_changes: HashMap<Uuid, Box<dyn Fn(u128) -> u128 + Send + 'static>>,
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
            match self.next_command(input, &results) {
                Either::Left(command) => {
                    results.push((command.clone(), self.controller.execute(command).await));
                }
                Either::Right(reply) => {
                    return reply;
                }
            }
        }
    }

    /// This function contains main application logic
    fn next_command(&mut self, input: &mut String, results: &Vec<(Command, CommandResult)>) -> Either<Command, String> {
        if results.len() == 0 {
            if let InputToken::Address(token, _) = consume_token(input) {
                Either::Left(match token.as_str() {
                    COMMAND_STATUS => Command::GetStatus,
                    COMMAND_PERIPHERALS => Command::ListPeripherals,
                    _ => {
                        let uuid = Uuid::from_slice(hex::decode(token).unwrap().as_slice());
                        info!("request for peripheral by service UUID: [{}]", hex::encode(uuid.shorten()));
                        Command::FindPeripheralByService(uuid)
                    }
                })
            } else {
                Either::Right("empty input".to_string())
            }
        } else {
            if let Some((command, result)) = results.last() {
                match result {
                    CommandResult::GetStatus(uptime, peripherals) => {
                        let mut status = format!("uptime {}", humantime::format_duration(*uptime));
                        if let Some(count) = peripherals {
                            status = status + &*format!(", {} peripherals connected", count);
                        }
                        Either::Right(status)
                    }
                    CommandResult::ListPeripherals(peripherals) => {
                        let descriptions = peripherals.into_iter()
                            .map(move |(p, _a)|
                                format!("{:?}", p))
                            .collect::<Vec<String>>();
                        Either::Right(format!(
                            "{} peripherals connected\n{}",
                            descriptions.len(),
                            descriptions.join("\n")
                        ))
                    }
                    CommandResult::FindPeripheral(info) =>
                        if let Some((p, a)) = info {
                            Either::Left(Command::ConnectToPeripheral(p.clone(), a.clone()))
                        } else {
                            Either::Right("peripheral match not found".to_string())
                        },
                    CommandResult::ConnectToPeripheral(peripheral) => {
                        info!("peripheral located '{:?}'", peripheral);
                        if input.is_empty() {
                            let cmt = results.iter().find_map(|(command, _)|
                                match command {
                                    Command::FindPeripheralByService(uuid) => Some(
                                        format!("by service UUID [{}]", hex::encode(uuid.shorten()))),
                                    _ => None
                                });

                            Either::Right(format!("peripheral [{}] {}", peripheral.id(), cmt.unwrap_or("".to_string())))
                        } else {
                            if let InputToken::Address(token, _) = consume_token(input) {
                                info!("peripheral [{}] searching for service {:?}", peripheral.id(), input);
                                Either::Left(Command::FindService(peripheral.clone(), token.to_string()))
                            } else {
                                Either::Right("connected to peripheral".to_string())
                            }
                        }
                    }
                    CommandResult::FindService(peripheral, service) => {
                        if let Some(service) = service {
                            if let InputToken::Address(token, range) = consume_token(input) {
                                info!("service found: '{:?}', searching for characteristic: {:?}", service, token);
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
                    CommandResult::FindCharacteristic(peripheral, characteristic) => {
                        if let Some(characteristic) = characteristic {
                            info!("characteristic found '{:?}'", characteristic);
                            let pending_range = if let Command::FindCharacteristic(_, _, _, uuid) = command {
                                self.pending_ranges.remove(uuid)
                            } else {
                                panic!("incorrect command mapping");
                            };
                            Either::Left({
                                if let Some(range) = pending_range {
                                    self.ranges.insert(characteristic.id(), range);
                                };
                                match consume_token(input) {
                                    InputToken::Negation => {
                                        self.pending_changes.insert(
                                            characteristic.id(),
                                            Box::new(|v| if v == 0 { 1 } else { 0 }),
                                        );
                                        Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
                                    }
                                    InputToken::Addition(delta, min, max) => {
                                        self.pending_changes.insert(
                                            characteristic.id(),
                                            Box::new(move |v| {
                                                let res = if delta.is_negative() {
                                                    v.saturating_sub(delta.saturating_neg() as u128)
                                                } else {
                                                    v.saturating_add(delta as u128)
                                                };
                                                if res < min {
                                                    min
                                                } else if res > max {
                                                    max
                                                } else {
                                                    res
                                                }
                                            }),
                                        );
                                        Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
                                    }
                                    InputToken::Address(token, _) => {
                                        let value = hex::decode(token).unwrap();
                                        if pending_range.is_some() {
                                            let v = bytes_to_u128(&value);
                                            self.pending_changes.insert(
                                                characteristic.id(),
                                                Box::new(move |_| v),
                                            );
                                            Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
                                        } else {
                                            Command::WriteCharacteristic(
                                                peripheral.clone(), characteristic.clone(), value,
                                            )
                                        }
                                    }
                                    InputToken::None => Command::ReadCharacteristic(peripheral.clone(), characteristic.clone())
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
                            let sliced_value = get_slice(value, range);
                            if let Some((_, change)) = self.pending_changes.remove_entry(id) {
                                info!("characteristic [{}] read ({}), now updating", characteristic.id(), hex::encode(sliced_value));
                                Either::Left({
                                    debug!("sliced value: {:?}", sliced_value);
                                    let updated_slice = change(bytes_to_u128(&sliced_value.to_vec())).to_le_bytes().to_vec();
                                    //TODO(df): limit value according to specified bytes range?
                                    debug!("updating value: {:?} - {:?} -> {:?}",
                                           hex::encode(value),
                                           hex::encode(&updated_slice),
                                           hex::encode(replace_slice(value, &updated_slice, range)));
                                    Command::WriteCharacteristic(
                                        peripheral.clone(), characteristic.clone(), replace_slice(value, &updated_slice, range))
                                })
                            } else {
                                info!("characteristic [{}] read range {:?}", characteristic.id(), range);
                                Either::Right(hex::encode(sliced_value))
                            }
                        } else {
                            Either::Right("characteristic read failed".to_string())
                        }
                    }
                    CommandResult::WriteCharacteristic(_, _, result) => {
                        Either::Right(format!("{:?}", result))
                    }
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
    let mut events = ShuttingDownStream::new(messages, shutdown);

    let mut session = Session::new(controller);

    while let Some((message, is_shutting_down)) = events.next().await {
        let r = session.process(message).await;
        let s = if is_shutting_down {
            r.to_owned()
        } else {
            r.to_owned() + "\n"
        };
        AsyncWriteExt::write_all(&mut stream, s.as_bytes()).await?;
        if is_shutting_down {
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
                info!("peer #{} disconnected, connected peers: {}", idx, peers.len());
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
            PeerEvent::Command { id, peer_id, shutdown: _ } =>
                if let Some(peer) = peers.get_mut(&peer_id) {
                    peer.send(id.clone()).await.unwrap();
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
                                .await.unwrap();
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
