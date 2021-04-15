use log::*;

use std::collections::hash_map::{Entry, HashMap};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use core_bluetooth::central::*;

use blemacd::{handlers::*, Position, central_async::*};

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    prelude::*,
    task,
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
use async_std::task::{Waker, JoinHandle};
use core_bluetooth::ManagerState;

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
    let mut peer_idx: u32 = 0;
    let (broker_sender, broker_receiver) = mpsc::unbounded();

    let broker_handle = task::spawn(broker_loop(broker_receiver));
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        info!("incoming connection: {:?}", stream.peer_addr()?);
        spawn_and_log_error(peer_reader_loop(broker_sender.clone(), stream, {
            peer_idx += 1;
            peer_idx
        }));
    }
    drop(broker_sender);
    broker_handle.await;
    Ok(())
}

async fn socket_connection_loop<P: AsRef<Path>>(path: P) -> Result<()> {
    let mut central_async = CentralAsync::new();
    task::block_on(
        async move {
            let state = central_async.wait(|event, central| {
                info!("--- {:?}", event);
                if let CentralEvent::ManagerStateChanged { new_state } = event {
                    match new_state {
                        ManagerState::Unsupported => {
                            eprintln!("Bluetooth is not supported on this system");
                        }
                        ManagerState::Unauthorized => {
                            eprintln!("The app is not authorized to use Bluetooth on this system");
                        }
                        ManagerState::PoweredOff => {
                            eprintln!("Bluetooth is disabled, please enable it");
                        }
                        ManagerState::PoweredOn => {
                            info!("bt is powered on, starting peripherals scan");
                            central.scan();
                        }
                        _ => {}
                    }

                    return Some(*new_state);
                }
                None
            }).await;

            info!("State received: {:?}", state);
        });
    Ok(())
}

async fn peer_reader_loop(mut broker: Sender<PeerEvent>, stream: UnixStream, idx: u32) -> Result<()> {
    let stream = Arc::new(stream);
    let reader = BufReader::new(&*stream);
    let mut lines = AsyncBufReadExt::lines(reader);

    let (_shutdown_sender, shutdown_receiver) = mpsc::unbounded::<Void>();
    broker
        .send(PeerEvent::NewPeer {
            idx,
            stream: stream.clone(),
            shutdown: shutdown_receiver,
        })
        .await
        .unwrap();

    while let Some(line) = lines.next().await {
        broker
            .send(PeerEvent::Command {
                id: line?,
                origin: idx,
                stream: stream.clone()
            })
            .await
            .unwrap();
    }

    Ok(())
}

async fn peer_writer_loop(
    messages: &mut Receiver<Reply>,
    stream: Arc<UnixStream>,
    shutdown: Receiver<Void>) -> Result<()> {
    let mut stream = &*stream;
    let mut events = FusedStream::new(Box::pin(messages), Box::pin(shutdown.fuse()));

    while let Some(event) = events.next().await {
        // this is SOMETIMES triggered with one liners
        if let Some(r) = &event.reply {
            let s = if event.shutdown {
                r.as_oneliner()
            } else {
                r.as_reply()
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
    let mut handler = InitHandler::new();

    let mut central_async = CentralAsync::new();
    let central = central_async.central.clone();
    let mut receiver = central_async.receiver.clone().fuse();

    let (disconnect_sender, mut disconnect_receiver) =
        mpsc::unbounded::<(u32, Receiver<Reply>, Arc<UnixStream>)>();
    let mut peers: HashMap<u32, Sender<Reply>> = HashMap::new();
    let mut events = events.fuse();

    loop {
        let peer_event = select! {
            central_event = receiver.next() => match central_event {
                None => break,
                Some(event) => {
                    let central = central.lock().unwrap();
                    let event = event.clone();
                    handler.handle_event(&event.lock().unwrap(), &central);
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
                    AsyncWriteExt::write_all(&mut stream, reply.as_oneliner().as_bytes())
                        .await.ok();
                }
                info!("peer #{} disconnection pending, currently connected peers: {}", idx, peers.len());
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
            PeerEvent::Command { id, origin, stream } => {
                if let Some(peer) = peers.get_mut(&origin) {
                    if let Some(message) = match id.clone().as_str() {
                        "status" => { // TODO(df): Move handler.execute out? (return Option<Command>)
                            Some(handler.execute(HandlerCommand::GetStatus(Box::new(|uptime, devices| {
                                let mut status = format!("uptime {}", humantime::format_duration(uptime));
                                if let Some((all, connected)) = devices {
                                    status = status + &*format!(", {} devices detected, {} connected", all, connected);
                                }
                                status
                            }))))
                        }
                        "all" => {
                            Some(handler.execute(HandlerCommand::ListDevices(Box::new(|devices| {
                                let devices = devices.values()
                                    .map(|p| p.to_string())
                                    .collect::<Vec<String>>();
                                format!(
                                    "{} devices total\n{}",
                                    devices.len(),
                                    devices.join("\n")
                                )
                            }))))
                        }
                        _ => {
                            let stream_arc = stream.clone();
                            handler.execute(HandlerCommand::FindDevice(id.clone(), Box::new(|s| s)))
                                .map_or(None, |peripheral_info| {
                                    let matched_id = peripheral_info.peripheral.id();
                                    info!("'{}' matched to {}, connecting", id.clone(), matched_id);

                                    // let central = central.clone();
                                    let mut receiver = central_async.receiver.clone();

                                    task::spawn(async move {
                                        //let central = central.lock().unwrap();
                                        use async_std::stream::StreamExt;
                                        let stream_arc = stream_arc.clone();

                                        // find_map is not a loop, it will return first result
                                        receiver.find_map(move |event| {
                                            let event = event.clone();
                                            let event: &CentralEvent = &event.lock().unwrap();

                                            match event {
                                                CentralEvent::PeripheralConnected { peripheral } => {
                                                    if peripheral.id() == matched_id {
                                                        info!("peripheral connected! {}", peripheral.id());
                                                        //drop(stream_ref);
                                                        return Some(());
                                                    }
                                                }
                                                CentralEvent::PeripheralConnectFailed { peripheral, error } => {
                                                    if peripheral.id() == matched_id {
                                                        warn!("failed to connect to peripheral {}", peripheral.id());
                                                        //cl.connect(&peripheral); // retry
                                                    }
                                                }
                                                _ => {}
                                            }

                                            //drop(stream_ref);
                                            None
                                        }).await
                                    });

                                    let mut central = central.lock().unwrap();
                                    central.connect(&peripheral_info.peripheral);
                                    None
                                    //handler.execute(HandlerCommand::ConnectToDevice(uuid, Box::new(|s| s)))
                                })
                            // .map(|connected_uuid| connected_uuid.to_string())
                        }
                    } {
                        info!("sending: {} to {}", message, id);
                        peer.send(Reply {
                            message,
                            origin: id.clone(),
                        })
                            .await
                            .unwrap()
                    }
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
                        // spawning separate loop to send data to the peer
                        // TODO(df): Prevent dropping stream if there are pending tasks (not only pending replies)

                        // `shutdown` receiver will receive `None` once reader loop will finish.
                        // On receiving `void` writer loop will quit. `disconnect_receiver` will send
                        // pending messages from `client_receiver` stream, but pending tasks
                        // won't be able to do much, except writing to stream directly.
                        // Besides, they do not know if disconnection flag already received or not
                        // (sending reply via as_reply or as_oneliner).
                        spawn_and_log_error(async move {
                            let res = peer_writer_loop(
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
    messages: Pin<Box<&'a mut Receiver<Reply>>>,
    shutdown: Pin<Box<Fuse<Receiver<Void>>>>,
}

impl<'a> FusedStream<'a> {
    fn new(
        messages: Pin<Box<&'a mut Receiver<Reply>>>,
        shutdown: Pin<Box<Fuse<Receiver<Void>>>>,
    ) -> FusedStream {
        Self { messages, shutdown }
    }
}

#[derive(Debug)]
struct FusedEvent {
    reply: Option<Reply>,
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
        origin: u32,
        stream: Arc<UnixStream>
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

    /*        task::block_on(socket_connection_loop(SOCKET_PATH))
                .map_err(|err| eprintln!("{:?}", err))
                .ok();
    */

    task::block_on(streams_accept_loop(SOCKET_PATH))
        .map_err(|err| eprintln!("{:?}", err))
        .ok();

    /*
     let mut app = App::new();
     task::block_on(async move {
         app.connect().await;
     });
     */

    cleanup_socket();
    info!("exiting application");

    /*
     let (central, receiver) = CentralManager::new();

    // https://rust-lang.github.io/async-book/06_multiple_futures/03_select.html

     let central = Arc::new(Mutex::new(central));
     let receiver = Arc::new(Mutex::new(receiver));

     let listener = UnixListener::bind(SOCKET_PATH).await?;
     info!("socket initialized");

     let mut incoming = listener.incoming();
     while let Some(stream) = incoming.next().await {
         let mut stream = stream?;


         // https://book.async.rs/tutorial/handling_disconnection.html#final-code

         info!("incoming stream?");
         match stream {
             Ok(stream) => {
                 if Arc::strong_count(&central) > 1 {
                     let mut writer = BufWriter::new(&stream);
                     writer.write_all(format!("just one connection please").as_bytes());
                     writer.flush();
                     stream.shutdown(Shutdown::Both);
                 } else {
                     info!("client connected");
                     // let (tx, rx) = mpsc::channel();
                     let central = Arc::clone(&central);
                     let receiver = Arc::clone(&receiver);
                     thread::spawn(move || {
                         let mut central = central.lock().unwrap();
                         let mut receiver = receiver.lock().unwrap();
                         handle_client(central.deref(), receiver.deref(), stream);
                         //let val = String::from("hi");
                         //tx.send(val).unwrap();
                         info!("client disconnected");
                     });
                     // let received = rx.recv().unwrap();
                     // println!("Got: {}", received);
                 }
             }
             Err(err) => {
                 println!("Error: {:?}", err);
                 break;
             }
         }

    } */
}
