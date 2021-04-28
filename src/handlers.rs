use log::*;

use std::collections::{HashMap, HashSet};
use std::process::exit;

use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::*;
use core_bluetooth::uuid::Uuid;
use core_bluetooth::*;

use core::fmt;
use std::time::{Instant, Duration};
use core_bluetooth::central::service::Service;
use std::fmt::{Debug};

use async_std::{future, task};
use std::sync::{Arc, Mutex};
use postage::prelude::Sink;
use std::pin::Pin;
use async_std::task::JoinHandle;
use futures::{StreamExt, select};
use async_std::sync::Weak;


const PERIPHERAL: &str = "fe3c678b-ab90-42ea-97d8-d13047ffdaa4";
// local hue lamp. THIS ID WILL BE DIFFERENT FOR ANOTHER DEVICE!
const PAIRING_SERVICE: &str = "932c32bd-0000-47a2-835a-a8d455b859dd";
// on/off service for Philips Hue BLE
const SERVICE: &str = "932c32bd-0000-47a2-835a-a8d455b859dd";
// on/off service for Philips Hue BLE
const CHARACTERISTIC: &str = "932c32bd-0002-47a2-835a-a8d455b859dd"; //  on/off characteristic

// --- Implementation draft ----------------------------------------------------
// https://stackoverflow.com/questions/27957103/how-do-i-create-a-heterogeneous-collection-of-objects
// https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=308ca372ab13fdb3ecec6f4a3702895d

#[derive(Debug, Clone)]
pub struct PeripheralInfo {
    pub peripheral: Peripheral,
    advertisement_data: AdvertisementData,
    //TODO(df): Peripheral can have multiple names?
}

impl fmt::Display for PeripheralInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display_name = if let Some(name) = self.advertisement_data.local_name() {
            format!("({})", name)
        } else {
            "".to_string()
        };
        write!(
            f,
            "[{}]{}: {} services",
            self.peripheral.id(),
            display_name,
            self.advertisement_data.service_uuids().len()
        )
    }
}

#[derive(Debug, Clone)]
enum CommandResult {
    GetStatus(Duration, Option<(usize, usize)>),
}

enum Command {
    GetStatus(postage::oneshot::Sender<CommandResult>)
}

pub struct Controller {
    sender: postage::mpsc::Sender<Command>,
}

impl Controller {
    pub async fn get_status(&mut self) -> (Duration, Option<(usize, usize)>) {
        let (mut sender, mut receiver) = postage::oneshot::channel();
        &self.sender.send(Command::GetStatus(sender)).await;
        // send command, receive answer, reply
        if let Some(CommandResult::GetStatus(duration, devices)) = receiver.next().await {
            (duration, devices)
        } else {
            panic!("Unknown command or empty result");
        }
    }
}

impl Clone for Controller {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone()
        }
    }
}

pub struct HandlerHandle {
    handle: JoinHandle<()>,
}

impl HandlerHandle {
    pub fn new() -> (Self, Controller) {
        let (command_sender, command_receiver) = postage::mpsc::channel::<Command>(100);
        (Self {
            handle: task::spawn(
                async move {
                    let (central, central_receiver) = CentralManager::new();
                    let mut handler = InitHandler {
                        started_at: Instant::now(),
                        state: ManagerState::Unknown,
                        next: None,
                    };
                    let mut command_receiver = command_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();
                    ////
                    loop {
                        select! {
                            command = command_receiver.next() => if let Some(command) = command {
                                match command {
                                Command::GetStatus(mut s) => {
                                    let (duration, devices) = handler.get_status();
                                    s.send(CommandResult::GetStatus(duration, devices)).await;
                                },
                            }
                            },
                            central_event = central_receiver.next() => if let Some(event) = central_event {
                                handler.handle_event(&event, &central);
                            }
                        }
                    }
                })
        },
         Controller { sender: command_sender })
    }


    // TODO(df): Locking and unlocking need to occur on the same thread.
    // So async function can't have .read() locking.
    /*
    pub async fn connect_to_device(&self, uuid: Uuid) -> Result<Peripheral, String> {
        if let Some(handler) = &self.0.read().unwrap().next {
            info!("invoking connect_to_device '{:?}'", uuid);
            handler.connect_to_device(uuid).await
        } else {
            panic!("Can't find device");
        }
    }
*/
    /*
        pub fn connect(&self, peripheral: &Peripheral) -> Pin<Box<dyn std::future::Future<Output=Option<Result<Peripheral, String>>> + Send>> {
            if let Some(handler) = &self.handler.read().unwrap().next {
                for peripheral in &handler.connected_peripherals {
                    if peripheral.id() == peripheral.id() {
                        return Box::pin(future::ready(Some(Ok(peripheral.clone()))));
                    }
                }

                // there are two ways of implementing this:
                // 1) store required uuids with associated futures in some map,
                //    check this map on every `handle_event`,
                //    complete futures if there's match
                // 2) async closure
                let receiver = handler.receiver.clone();
                let id = peripheral.id().clone();

                use async_std::stream::StreamExt;

                info!("starting find_map");
                let fut = Box::pin(async move {
                    let mut receiver = receiver.clone();
                    receiver.find_map(move |event| {
                        info!("processing event {:?}", event);
                        {
                            let event = event.clone();
                            match &*event.lock().unwrap() {
                                CentralEvent::PeripheralConnected { peripheral } => {
                                    if peripheral.id() == id {
                                        info!("peripheral connected! {}", peripheral.id());
                                        return Some(Ok(peripheral.clone()));
                                    }
                                }
                                CentralEvent::PeripheralConnectFailed { peripheral, error } => {
                                    if peripheral.id() == id {
                                        warn!("failed to connect to peripheral {}", peripheral.id());
                                        // we may want to retry connection
                                        // TODO(df): Output error
                                        return Some(Err("error".to_string()));
                                    }
                                }
                                _ => {}
                            };
                        }

                        None
                    }).await
                });
                self.central.lock().unwrap().connect(&peripheral);
                fut
            } else {
                Box::pin(future::ready(Some(Err("no handler".to_string()))))
            }
        }

        pub fn find_device(&self, uuid_substr: String) -> Option<PeripheralInfo> {
            if let Some(handler) = &self.handler.read().unwrap().next {
                handler.find_device(uuid_substr)
            } else {
                panic!("Can't find device");
            }
        }

     */
}

struct InitHandler<'a> {
    started_at: Instant,
    state: ManagerState,
    next: Option<RootHandler<'a>>,
}

impl InitHandler<'_>
{
    pub fn get_status(&self) -> (Duration, Option<(usize, usize)>) {
        (
            Duration::new(self.started_at.elapsed().as_secs(), 0),
            if let Some(handler) = &self.next {
                Some((handler.peripherals.len(), handler.connected_peripherals.len()))
            } else {
                None
            }
        )
    }

    pub fn list_devices(&self) -> HashMap<Uuid, PeripheralInfo> {
        if let Some(handler) = &self.next {
            handler.peripherals.clone()
        } else {
            panic!("Can't list devices");
        }
    }

    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        if let CentralEvent::ManagerStateChanged { new_state } = event {
            self.state = *new_state;
            match new_state {
                ManagerState::Unsupported => {
                    eprintln!("Bluetooth is not supported on this system");
                    exit(1);
                }
                ManagerState::Unauthorized => {
                    eprintln!("The app is not authorized to use Bluetooth on this system");
                    exit(1);
                }
                ManagerState::PoweredOff => {
                    eprintln!("Bluetooth is disabled, please enable it");
                    // TODO(df): Clean up child handler
                }
                ManagerState::PoweredOn => {
                    info!("bt is powered on, starting peripherals scan");
                    central.scan();
                    self.next = Some(RootHandler::new());

                    //TODO(df): Run different type of peripherals search depending on params

                    // match &self.peripheral_uuid {
                    //    Some(uuid) => central.get_peripherals(&[uuid.parse().unwrap()]),
                    //    None => central.scan(),
                    //}
                    //central.get_peripherals_with_services(&[SERVICE.parse().unwrap()]) // TODO(df): Implement connection by service uuid
                }
                _ => {}
            }
        };

        if let Some(handler) = &mut self.next {
            handler.handle_event(event, central);
        }
    }
}

struct RootHandler<'a> {
    connected_peripherals: HashSet<Peripheral>,
    peripherals: HashMap<Uuid, PeripheralInfo>,
    sender: postage::broadcast::Sender<Arc<Mutex<CentralEvent>>>,
    receiver: postage::broadcast::Receiver<Arc<Mutex<CentralEvent>>>,
    next: Option<DeviceHandler<'a>>,
}

impl RootHandler<'_> {
    fn new() -> Self {
        let (sender, receiver) = postage::broadcast::channel(100);
        Self {
            sender,
            receiver,
            connected_peripherals: HashSet::new(),
            peripherals: HashMap::new(),
            next: None,
        }
    }

    fn find_device(&self, uuid_substr: String) -> Option<PeripheralInfo> {
        let s = uuid_substr.as_str();
        for (uuid, peripheral) in &self.peripherals {
            let uuid_string = uuid.to_string();
            if uuid_string.contains(s) {
                return Some(peripheral.clone());
            }
        }
        None
    }

    /*
        fn execute<T>(&mut self, command: HandlerCommand) -> T {
            match command {
                HandlerCommand::FindDevice(substring) => {
                    if let Some(handler) = &mut self.next {
                        None // TODO(df): Add matching for next
                    } else {
                        let s = substring.as_str();
                        let mut result = None;
                        for (uuid, peripheral) in &self.peripherals {
                            let uuid_string = uuid.to_string();
                            if uuid_string.contains(s) {
                                result = Some(peripheral.clone());
                                break;
                            }
                        }
                        result
                    }
                }
                HandlerCommand::ConnectToDevice(uuid) => {
                    if let Some(handler) = &mut self.next {
                        err("no handler".to_string())
                    } else {
                        for peripheral in &self.connected_peripherals {
                            if peripheral.id() == uuid {
                                return ok(peripheral.clone());
                            }
                        }

                        // there are two ways of implementing this:
                        // 1) store required uuids with associated futures in some map,
                        //    check this map on every `handle_event`,
                        //    complete futures if there's match
                        // 2) async closure
                        let mut receiver = &self.receiver.clone();
                        let id = uuid.clone();

                        let fut = async move {
                            use async_std::stream::StreamExt;

                            if let Some(reply) = receiver.find_map(move |event| {
                                let event: &CentralEvent = &event.lock().unwrap();

                                match event {
                                    CentralEvent::PeripheralConnected { peripheral } => {
                                        if peripheral.id() == id {
                                            info!("peripheral connected! {}", peripheral.id());
                                            return Some(Ok(peripheral.clone()));
                                        }
                                    }
                                    CentralEvent::PeripheralConnectFailed { peripheral, error } => {
                                        if peripheral.id() == id {
                                            warn!("failed to connect to peripheral {}", peripheral.id());
                                            // we may want to retry connection
                                            return Some(Err(error.unwrap().to_string()));
                                        }
                                    }
                                    _ => {}
                                }

                                None
                            }).await {
                                reply
                            } else {
                                Err("can't find device".to_string())
                            }
                        };

                        fut
                    }
                }
                _ => {
                    //   if let Some(&mut handler) = self.next {
                    //       handler.execute(command);
                    //   } else {
                    panic!("Can't execute command {}", command.name());
                    //   }
                }
            }
        }
    */

    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        //let event_to_send = event.clone();
        debug!("handling_event, {:?}", event);
        match event {
            CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                rssi: _,
            } => {
                debug!("[PeripheralDiscovered]: {}", self.peripherals.len());
                info!("[PeripheralDiscovered]: {}", self.peripherals.len());
                self.peripherals.insert(
                    peripheral.id(),
                    PeripheralInfo {
                        peripheral: peripheral.clone(),
                        advertisement_data: advertisement_data.clone(),
                    },
                );
            }
            /*
                        CentralEvent::GetPeripheralsResult {
                            peripherals,
                            tag: _,
                        } => {
                            for peripheral in peripherals {
                                debug!("[GetPeripheralsResult]: {}", peripheral.id());
                                if self.connected_peripherals.insert(peripheral.clone()) {
                                    //println!("connecting to {})", peripheral.id());
                                    central.connect(&peripheral);
                                }
                            }
                        }*/
            CentralEvent::PeripheralConnected { peripheral } => {
                self.connected_peripherals.insert(peripheral.clone());
                info!("registered peripheral {}, total {}", peripheral.id(), self.connected_peripherals.len());
                peripheral.discover_services_with_uuids(&[SERVICE.parse().unwrap()]);
            }
            CentralEvent::PeripheralDisconnected {
                peripheral,
                error: _,
            } => {
                self.connected_peripherals.remove(&peripheral);
                info!("unregistered peripheral {}, total {}", peripheral.id(), self.connected_peripherals.len());
                //central.connect(&peripheral);
            }
            CentralEvent::GetPeripheralsWithServicesResult {
                peripherals,
                tag: _,
            } => {
                debug!("[GetPeripheralsWithServicesResult]: {}", peripherals.len());
                for peripheral in peripherals {
                    //println!("Discovered with result: {}", peripheral.id());
                    /*    if self.connected_peripherals.insert(p.clone()) {
                        debug!("connecting to {})", p.id());
                        self.central.connect(&p);
                    }*/
                }
            }

            _ => {}
        }
        //self.sender.blocking_send(event_to_send).unwrap();
    }
}

struct DeviceHandler<'a> {
    peripheral: &'a Peripheral,
    services: HashMap<Uuid, Service>,
}

impl<'a> DeviceHandler<'a> {
    fn new(peripheral: &'a Peripheral) -> Self {
        Self {
            peripheral,
            services: HashMap::new(),
        }
    }

    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        match event {
            CentralEvent::ServicesDiscovered {
                peripheral,
                services,
            } => {
                //println!("Services discovered for peripheral {}", peripheral.id());
                if let Ok(services) = services {
                    //println!("Starting characteristics discovery {}", services.len());
                    for service in services {
                        peripheral.discover_characteristics_with_uuids(
                            &service,
                            &[CHARACTERISTIC.parse().unwrap()],
                        );
                    }
                }
            }
            CentralEvent::CharacteristicsDiscovered {
                peripheral,
                service: _,
                characteristics,
            } => {
                match characteristics {
                    Ok(chars) => {
                        //info!("subscribing to characteristic {} of {}", chars[0].id(), peripheral.id());
                        peripheral.subscribe(&chars[0]);
                    }
                    Err(err) => error!(
                        "couldn't discover characteristics of {}: {}",
                        peripheral.id(),
                        err
                    ),
                }
            }
            CentralEvent::CharacteristicValue {
                peripheral,
                characteristic: _,
                value,
            } => {
                if let Ok(value) = value {
                    let now = chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]");

                    let t = i16::from_le_bytes([value[0], value[1]]) as f64 / 100.0;
                    let rh = value[2];
                    //println!("{} #{}: t = {} C, rh = {}%", now, peripheral.id(), t, rh);
                }
            }

            _ => {}
        }
    }
}
