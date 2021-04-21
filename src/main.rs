use log::*;

use std::collections::hash_map::{Entry, HashMap};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use core_bluetooth::central::*;
use postage::prelude::Sink;

use blemacd::{handlers::*, Position};

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    prelude::*,
    sync::RwLock,
    task,
    task::{Waker, JoinHandle},
};
use futures::{
    channel::mpsc, select, pin_mut, sink::SinkExt, stream::Fuse, AsyncBufReadExt, AsyncWriteExt,
    StreamExt,
    future::{Future, BoxFuture, FutureExt, LocalBoxFuture},
    task::{waker_ref, ArcWake},
};

use env_logger::{Builder, Env};
use std::sync;

use std::time::{Duration, Instant};
use core_bluetooth::uuid::Uuid;
use core_bluetooth::central::service::Service;
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::characteristic::{Characteristic, WriteKind};
use core_bluetooth::error::Error;
use std::collections::HashSet;
use core_bluetooth::ManagerState;
use std::borrow::Borrow;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Sender<T> = mpsc::UnboundedSender<T>;
type Receiver<T> = mpsc::UnboundedReceiver<T>;

#[derive(Debug)]
enum Void {}

const SOCKET_PATH: &str = "/tmp/blemacd.sock";

const PERIPHERAL: &str = "fe3c678b-ab90-42ea-97d8-d13047ffdaa4";
// local hue lamp. THIS ID WILL BE DIFFERENT FOR ANOTHER DEVICE!
const PAIRING_SERVICE: &str = "932c32bd-0000-47a2-835a-a8d455b859dd";
// on/off service for Philips Hue BLE
const SERVICE: &str = "932c32bd-0000-47a2-835a-a8d455b859dd";
// on/off service for Philips Hue BLE
const CHARACTERISTIC: &str = "932c32bd-0002-47a2-835a-a8d455b859dd"; //  on/off characteristic


async fn streams_accept_loop<P: AsRef<Path>>(path: P) -> Result<()> {
    // https://docs.rs/async-std/0.99.3/async_std/os/unix/net/struct.UnixListener.html
    let listener = UnixListener::bind(path).await?;
    let mut peer_id: u32 = 0;
    let (broker_sender, broker_receiver) = mpsc::unbounded();

    let broker_handle = task::spawn(broker_loop(broker_receiver));
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        info!("incoming connection: {:?}", stream.peer_addr()?);
        spawn_and_log_error(peer_reader_loop(broker_sender.clone(), stream, {
            peer_id += 1;
            peer_id
        }));
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

async fn peer_writer_loop(
    handler: Arc<HandlerHandle<'_>>,
    messages: &mut Receiver<String>,
    stream: Arc<UnixStream>,
    shutdown: Receiver<Void>) -> Result<()> {
    let mut stream = &*stream;
    let mut events = FusedStream::new(Box::pin(messages), Box::pin(shutdown.fuse()));

    while let Some(event) = events.next().await {
        if let Some(r) = &event.reply {
            let r = if let Some(reply) = match r.as_str() {
                "status" => { // TODO(df): Move handler.execute out? (return Option<Command>)
                    info!("incoming status command");
                    let (uptime, devices) = handler.get_status();
                    let mut status = format!("uptime {}", humantime::format_duration(uptime));
                    if let Some((all, connected)) = devices {
                        status = status + &*format!(", {} devices detected, {} connected", all, connected);
                    }
                    Some(status)
                }
                "all" => {
                    let devices = handler.list_devices().values()
                        .map(|p| p.to_string())
                        .collect::<Vec<String>>();
                    Some(format!(
                        "{} devices total\n{}",
                        devices.len(),
                        devices.join("\n")
                    ))
                }
                _ => {
                    if let Some(peripheral_info) = {
                        handler.find_device(r.clone())
                    } {
                        let matched_id = peripheral_info.peripheral.id();
                        Some(format!("'{}' matched to {}, connecting (not really)", r.clone(), matched_id));

                        // TODO(df): We are holding reference to the handler here.
                        // It makes impossible to update handler with new event, so its a deadlock.
                        // Solution would be to wrap handler inside Arc<Mutex<>> and expose the wrapper,
                        // see https://users.rust-lang.org/t/mutable-struct-fields-with-async-await/45395/7

                        if let Some(result) = handler.connect_to_device(matched_id).await {
                            match result {
                                Ok(peripheral) => Some(format!("connected to peripheral {:?}", peripheral)),
                                Err(error) => Some(error)
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            } {
                reply
            } else {
                "oops, nothing".to_string()
            };

            let s = if event.shutdown {
                r.to_owned() + ", bye!"
            } else {
                r.to_owned() + ", but wait"
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


fn wrap_central() -> (JoinHandle<()>, CentralManager, postage::broadcast::Receiver<Arc<Mutex<CentralEvent>>>) {
    let (mut sender, receiver) = postage::broadcast::channel(100);
    let (central, mut central_receiver) = CentralManager::new();

    (task::spawn(
        async move {
            while let Some(event) = central_receiver.next().await {
                let event = Arc::new(Mutex::new(event));
                sender.send(event).await.unwrap();
            }
        }),
     central, receiver)
}

// Base logic taken from https://book.async.rs/tutorial/handling_disconnection.html#final-code
async fn broker_loop(events: Receiver<PeerEvent>) {
    let handler = Arc::new(HandlerHandle::new());

    let (_, central, central_receiver) = wrap_central();

    let mut receiver = central_receiver.clone().fuse();

    let (disconnect_sender, mut disconnect_receiver) =
        mpsc::unbounded::<(u32, Receiver<String>, Arc<UnixStream>)>();
    let mut peers: HashMap<u32, Sender<String>> = HashMap::new();
    let mut events = events.fuse();

    loop {
        let peer_event = select! {
            central_event = receiver.next() => match central_event {
                None => break,
                Some(event) => {
                    handler.handle_event(event.clone(), &central);
                    continue;
                }
            },
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


        //TODO(df): peer_event should be dispatched and processed in its own thread
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
                    /*
                if let Some(message) = match id.clone().as_str() {
                    _ => {
                        if let Some(peripheral_info) = {
                            let handler = &*handler.lock().unwrap();
                            handler.find_device(id.clone())
                        } {
                            let matched_id = peripheral_info.peripheral.id();
                            info!("'{}' matched to {}, connecting", id.clone(), matched_id);

                           // let handler = &*handler.lock().unwrap();
                           // if let Ok(peripheral) = handler.connect_to_device(matched_id).await {
                           //    info!("connected to peripheral {:?}", peripheral);
                           // }
                            None
                        } else {
                            None
                        }
*/
                    /*
                    let shutdown = shutdown.clone();
                    // TODO(df): Async GetDevice command in handler.
                    handler.execute(HandlerCommand::FindDevice(id.clone(), Box::new(|s| s)))
                        .map_or(None, |peripheral_info| {
                            let matched_id = peripheral_info.peripheral.id();
                            info!("'{}' matched to {}, connecting", id.clone(), matched_id);

                            let mut receiver = central_receiver.clone();
                            let mut peer = peer.clone();
                            let id = id.clone();

                            task::spawn(async move {
                                use async_std::stream::StreamExt;
                                let shutdown = shutdown.clone();

                                if let Some(reply) = receiver.find_map(move |event| {
                                    let event: &CentralEvent = &event.lock().unwrap();

                                    match event {
                                        CentralEvent::PeripheralConnected { peripheral } => {
                                            if peripheral.id() == matched_id {
                                                info!("peripheral connected! {}", peripheral.id());
                                                return Some(Reply {
                                                    message: peripheral.id().to_string(),
                                                    origin: id.clone(),
                                                });
                                            }
                                        }
                                        CentralEvent::PeripheralConnectFailed { peripheral, error } => {
                                            if peripheral.id() == matched_id {
                                                warn!("failed to connect to peripheral {}", peripheral.id());
                                                // we may want to retry connection
                                                return Some(Reply {
                                                    message: "failed".to_string(),
                                                    origin: id.clone(),
                                                });
                                            }
                                        }
                                        _ => {}
                                    }

                                    None
                                }).await {
                                    peer.send(reply).await.unwrap();
                                }
                            });

                            let mut central = central.lock().unwrap();
                            central.connect(&peripheral_info.peripheral);
                            None
                        })
                    */
                }
            }
            PeerEvent::NewPeer {
                idx,
                stream,
                shutdown,
            } => {
                info!("peer #{} connected, {} connected peers", idx, peers.len());
                match peers.entry(idx) {
                    Entry::Occupied(..) => (),
                    Entry::Vacant(entry) => {
                        let (client_sender, mut client_receiver) = mpsc::unbounded();
                        entry.insert(client_sender);
                        let mut disconnect_sender = disconnect_sender.clone();

                        let handler = handler.clone();
                        spawn_and_log_error(async move {
                            let res = peer_writer_loop(
                                handler,
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
