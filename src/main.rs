use log::*;

use std::collections::hash_map::{Entry, HashMap};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use core_bluetooth::central::*;

use blemacd::{handlers::*, Position};

use async_std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    prelude::*,
    task,
};
use futures::{
    channel::mpsc, select, sink::SinkExt, stream::Fuse, AsyncBufReadExt, AsyncWriteExt, FutureExt,
    StreamExt,
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

/*
fn handle_client(central: &CentralManager, receiver: &Receiver<CentralEvent>, mut stream: UnixStream) {
let reader = BufReader::new(&stream);
let mut writer = BufWriter::new(&stream);

// --- Populate handlers according to command line parameters

let mut handlers: HashMap<Position, EventHandler> = HashMap::new();

handlers.insert(Position::Root,
                EventHandler::Connection(ConnectionHandler { peripheral_uuid: None }));

// --- Run events receiving loop

while let Ok(event) = receiver.recv() {
    info!("Active handlers: {}", handlers.len());
    let mut operations: Vec<HandlerOperation> = vec![];
    for position in Position::iter() {
        match handlers.get_mut(&position) {
            Some(handler) =>
                operations.push(handler.handle_event(&event, &central)),
            None => {}
        }
    }
    for operation in operations.iter_mut() {
        match operation {
            HandlerOperation::Insert(position, handler) => {
                handlers.insert(*position.deref(), handler.take().unwrap());
            }
            _ => {}
        }
    }
    /*
            println!(" --- peripherals list ---");
            for p in &peripherals {
                println!("   {}: {}",
                         p.peripheral.id(),
                         p.advertisement_data.local_name().unwrap_or("*** none ***"));

            }*/
}


}
*/

async fn accept_loop<P: AsRef<Path>>(path: P) -> Result<()> {
    let listener = UnixListener::bind(path).await?;
    let mut connection_idx: u32 = 0;
    let (broker_sender, broker_receiver) = mpsc::unbounded();
    let broker_handle = task::spawn(broker_loop(broker_receiver));
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        info!("incoming connection: {:?}", stream.peer_addr()?);
        spawn_and_log_error(connection_loop(broker_sender.clone(), stream, {
            connection_idx += 1;
            connection_idx
        }));
    }
    drop(broker_sender);
    broker_handle.await;
    Ok(())
}

async fn connection_loop(mut broker: Sender<Event>, stream: UnixStream, idx: u32) -> Result<()> {
    let stream = Arc::new(stream);
    let reader = BufReader::new(&*stream);
    let mut lines = AsyncBufReadExt::lines(reader);

    let (_shutdown_sender, shutdown_receiver) = mpsc::unbounded::<Void>();
    broker
        .send(Event::NewPeer {
            idx,
            stream: Arc::clone(&stream),
            shutdown: shutdown_receiver,
        })
        .await
        .unwrap();

    while let Some(line) = lines.next().await {
        broker
            .send(Event::Command {
                id: line?,
                origin: idx,
            })
            .await
            .unwrap();
    }

    Ok(())
}

async fn central_events_loop(mut handlers: Arc<Mutex<HashMap<Position, EventHandler>>>) {
    let (central, receiver) = CentralManager::new();

    let handlers = Arc::clone(&handlers);

    handlers.lock().unwrap().insert(
        Position::Root,
        EventHandler::Connection(ConnectionHandler::new()),
    );

    while let Ok(event) = receiver.recv() {
        //info!("Active handlers: {}", handlers.len());
        let mut operations: Vec<HandlerOperation> = vec![];
        for position in Position::iter() {
            match handlers.lock().unwrap().get_mut(&position) {
                Some(handler) => operations.push(handler.handle_event(&event, &central)),
                None => {}
            }
        }
        for operation in operations.iter_mut() {
            match operation {
                HandlerOperation::Insert(position, handler) => {
                    handlers
                        .lock()
                        .unwrap()
                        .insert(*position.deref(), handler.take().unwrap());
                }
                _ => {}
            }
        }
    }
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

async fn broker_loop(events: Receiver<Event>) {
    let mut handlers: HashMap<Position, EventHandler> = HashMap::new();

    let handlers = Arc::new(Mutex::new(handlers));
    task::spawn(central_events_loop(handlers.clone()));

    let (disconnect_sender, mut disconnect_receiver) =
        mpsc::unbounded::<(u32, Receiver<Reply>, Arc<UnixStream>)>();
    let mut peers: HashMap<u32, Sender<Reply>> = HashMap::new();
    let mut events = events.fuse();
    loop {
        let event = select! {
            event = events.next().fuse() =>
                match event {
                    None => break,
                    Some(event) => event,
                }
            ,
            disconnect = disconnect_receiver.next() => {
                let (idx, mut pending_replies, stream) = disconnect.unwrap();
                assert!(peers.remove(&idx).is_some());

                let mut stream = &*stream;
                while let Ok(Some(reply)) = pending_replies.try_next() {
                    AsyncWriteExt::write_all(&mut stream, reply.as_oneliner().as_bytes())
                        .await.ok();
                }
                info!("peer #{} disconnected, currently connected peers: {}", idx, peers.len());
                continue;
            }
        };

        match event {
            Event::Command { id, origin } => {
                if let Some(peer) = peers.get_mut(&origin) {
                    if let Some(message) = match id.clone().as_str() {
                        "status" => {
                            let mut handlers = handlers.lock().unwrap();
                            if let EventHandler::Connection(handler) =
                                handlers.get_mut(&Position::Root).unwrap()
                            {
                                Some(format!("{:?}", handler.status))
                            } else {
                                None
                            }
                        }
                        "devices" => {
                            let mut handlers = handlers.lock().unwrap();
                            if let EventHandler::Peripherals(handler) =
                                handlers.get_mut(&Position::Devices).unwrap()
                            {
                                let devices = handler
                                    .peripherals
                                    .values()
                                    .map(|p| p.to_string())
                                    .collect::<Vec<String>>();
                                Some(format!(
                                    "{} devices total\n{}",
                                    devices.len(),
                                    devices.join("\n")
                                ))
                            } else {
                                None
                            }
                        }
                        _ => None,
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
            Event::NewPeer {
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
                        // spawning separate thread to send data to peers
                        spawn_and_log_error(async move {
                            let res = connection_writer_loop(
                                &mut client_receiver,
                                stream.clone(),
                                shutdown,
                            )
                            .await;

                            // One-liner will terminate connection_writer_loop fast enough
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

async fn connection_writer_loop(
    messages: &mut Receiver<Reply>,
    stream: Arc<UnixStream>,
    shutdown: Receiver<Void>,
) -> Result<()> {
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
enum Event {
    NewPeer {
        idx: u32,
        stream: Arc<UnixStream>,
        shutdown: Receiver<Void>,
    },
    Command {
        id: String,
        origin: u32,
    },
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
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

    task::block_on(accept_loop(SOCKET_PATH))
        .map_err(|err| eprintln!("{:?}", err))
        .ok();

    cleanup_socket();
    info!("exiting application");

    /*
     let (central, receiver) = CentralManager::new();

    // https://rust-lang.github.io/async-book/06_multiple_futures/03_select.html
    // https://users.rust-lang.org/t/multiple-receiver-mpsc-channel-using-arc-mutex-receiver/37798

     let central = Arc::new(Mutex::new(central));
     let receiver = Arc::new(Mutex::new(receiver));

     let listener = UnixListener::bind(SOCKET_PATH).await?;
     info!("socket initialized");

     let mut incoming = listener.incoming();
     while let Some(stream) = incoming.next().await {
         let mut stream = stream?;

         // https://docs.rs/async-std/0.99.3/async_std/os/unix/net/struct.UnixListener.html
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
