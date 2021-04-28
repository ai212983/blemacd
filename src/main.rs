use log::*;

use std::collections::hash_map::{Entry, HashMap};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use blemacd::handlers::*;

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    prelude::*,
    task,
};

use futures::{
    channel::mpsc, select, sink::SinkExt, stream::Fuse, AsyncBufReadExt, AsyncWriteExt,
    StreamExt,
    future::{Future, FutureExt},
};

use env_logger::{Builder, Env};

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

const COMMAND_STATUS: &str = "status";
const COMMAND_ALL_DEVICES: &str = "all";


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

// LEFT_HERE(df)
// We are passing handler to the thread, which is communicating by sharing data (control object), not by messages.
// Proper solution would be to have cloneable struct to wrap channel and returning async results
// Maybe copy some code from HandlerHandle
async fn peer_writer_loop(
    mut controller: Controller,
    messages: &mut Receiver<String>,
    stream: Arc<UnixStream>,
    shutdown: Receiver<Void>) -> Result<()> {
    let mut stream = &*stream;
    let mut events = FusedStream::new(Box::pin(messages), Box::pin(shutdown.fuse()));

    while let Some(event) = events.next().await {
        if let Some(r) = &event.reply {
            let r = if let Some(reply) = match r.as_str() {
                COMMAND_STATUS => { // TODO(df): Move handler.execute out? (return Option<Command>)
                    let (uptime, devices) = controller.get_status().await;
                    let mut status = format!("uptime {}", humantime::format_duration(uptime));
                    if let Some((all, connected)) = devices {
                        status = status + &*format!(", {} devices detected, {} connected", all, connected);
                    }
                    Some(status)
                }
                COMMAND_ALL_DEVICES => {
                    //let devices = handler.read().unwrap().list_devices().values()
                    let devices = controller.list_devices().await
                        .values()
                        .map(|p| p.to_string())
                        .collect::<Vec<String>>();
                    Some(format!(
                        "{} devices total\n{}",
                        devices.len(),
                        devices.join("\n")
                    ))
                }
                _ => {
                    info!("'incoming match command");
                    /*  let handler = handler.read().unwrap();
                      if let Some(peripheral_info) = {
                          handler.find_device(r.clone())
                      } {
                          let matched_id = peripheral_info.peripheral.id();
                          info!("'{}' matched to {}, connecting (not really)", r.clone(), matched_id);

                          // TODO(df): We are holding reference to the handler here.
                          // It makes impossible to update handler with new event, so its a deadlock.
                          // Solution would be to wrap handler inside Arc<Mutex<>> and expose the wrapper,
                          // see https://users.rust-lang.org/t/mutable-struct-fields-with-async-await/45395/7

                          if let Some(result) = handler.connect(&peripheral_info.peripheral).await {
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

                     */
                    None
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


// Base logic taken from https://book.async.rs/tutorial/handling_disconnection.html#final-code
async fn broker_loop(events: Receiver<PeerEvent>) {
    let (_handler, controller) = HandlerHandle::new();

    let (disconnect_sender, mut disconnect_receiver) =
        mpsc::unbounded::<(u32, Receiver<String>, Arc<UnixStream>)>();
    let mut peers: HashMap<u32, Sender<String>> = HashMap::new();
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
