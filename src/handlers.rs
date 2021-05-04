use log::*;

use std::collections::{HashMap, HashSet};
use std::process::exit;

use core_bluetooth::{*, central::{*,
                                  peripheral::Peripheral,
                                  service::Service,
                                  characteristic::Characteristic},
                     uuid::Uuid};
use core::fmt;
use std::time::{Instant, Duration};
use std::fmt::{Debug};

use std::sync::{Arc, Mutex};
use postage::{prelude::Sink, *};
use async_std::{prelude::*, stream, task, task::{Context, JoinHandle, Poll}};
use futures::{StreamExt, select};
use std::cell::RefCell;
use std::pin::Pin;
use futures::future::Either;


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
pub enum CommandResult {
    GetStatus(Duration, Option<(usize, usize)>),
    ListConnectedDevices(HashSet<Peripheral>),
    ListDevices(HashMap<Uuid, PeripheralInfo>),
    FindPeripheral(Option<PeripheralInfo>),
    ConnectToPeripheral(Peripheral),
    FindService(Option<Service>),
}

pub enum Command {
    GetStatus,
    ListDevices,
    ListConnectedDevices,
    FindPeripheral(String),
    ConnectToPeripheral(Peripheral),
    FindService(Peripheral, String),
}

pub struct Controller {
    sender: mpsc::Sender<(Command, oneshot::Sender<CommandResult>)>,
}

impl Controller {
    pub async fn execute(&mut self, command: Command) -> CommandResult {
        let (sender, mut receiver) = oneshot::channel();
        &self.sender.send((command, sender)).await;
        if let Some(reply) = receiver.next().await {
            reply
        } else {
            panic!("Unexpected 'None' result, sender was prematurely dropped?");
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
        let (command_sender, command_receiver) = mpsc::channel::<(Command, oneshot::Sender<CommandResult>)>(100);
        (Self {
            handle: task::spawn(
                async move {
                    let (central, central_receiver) = CentralManager::new();
                    let mut handler = InitHandler {
                        started_at: Instant::now(),
                        state: ManagerState::Unknown,
                        next: None,
                        handlers: vec![],
                    };
                    let mut command_receiver = command_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();
                    ////
                    loop {
                        select! {
                            command = command_receiver.next() => if let Some((command, sender)) = command {
                                handler.execute(command, sender, &central)
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
}

struct InitHandler<'a> {
    started_at: Instant,
    state: ManagerState,
    next: Option<RootHandler<'a>>,
    handlers: Vec<(Box<dyn Fn(&CentralEvent) -> Option<CommandResult> + Send>, RefCell<oneshot::Sender<CommandResult>>)>,
}

impl InitHandler<'_> {
    fn execute(&mut self, command: Command, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager) -> () {
        match command {
            Command::GetStatus => {
                sender.blocking_send(CommandResult::GetStatus(
                    Duration::new(self.started_at.elapsed().as_secs(), 0),
                    if let Some(handler) = &self.next {
                        Some((handler.peripherals.len(), handler.connected_peripherals.len()))
                    } else {
                        None
                    },
                ))
            }
            Command::ListConnectedDevices => sender.blocking_send(CommandResult::ListConnectedDevices(
                self.next.as_ref().expect("Not initialized").connected_peripherals.clone()
            )),
            Command::ListDevices => sender.blocking_send(CommandResult::ListDevices(
                self.next.as_ref().expect("Not initialized").peripherals.clone()
            )),
            Command::FindPeripheral(uuid_substr) => {
                let device = self.next.as_ref().expect("Not initialized").find_device(uuid_substr);
                sender.blocking_send(CommandResult::FindPeripheral(device))
            }
            Command::ConnectToPeripheral(peripheral) => Ok({
                let id = peripheral.id().clone();
                let device = self.next.as_ref().expect("Not initialized").get_connected_device(id);

                if let Some(peripheral) = device {
                    sender.blocking_send(CommandResult::ConnectToPeripheral(peripheral.clone()));
                } else {
                    &self.handlers.push((Box::new(move |event| {
                        //TODO(df): handle CentralEvent::PeripheralConnectFailed { peripheral, error }
                        if let CentralEvent::PeripheralConnected { peripheral } = event {
                            if peripheral.id() == id {
                                //TODO(df): Check if thread is released after disconnect
                                info!("connected peripheral {:?}", peripheral);
                                return Some(CommandResult::ConnectToPeripheral(peripheral.clone()));
                            }
                        }
                        None
                    }), RefCell::new(sender)));
                    central.connect(&peripheral);
                }
            }),
            Command::FindService(peripheral, uuid_substr) => Ok({
                let id = peripheral.id();
                &self.handlers.push((Box::new(move |event| {
                    if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
                        if peripheral.id() == id {
                            //TODO(df): Check if thread is released after disconnect
                            info!("connected services: {:?}", services);
                            if let Ok(services) = services {
                                return Some(CommandResult::FindService(None));
                            } else {
                                return Some(CommandResult::FindService(None));
                            }
                        }
                    }
                    None
                }), RefCell::new(sender)));
                peripheral.discover_services();
            })
        };
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
                    //TODO(df): Run different type of peripherals search depending on params, for example, by service uuid
                    //central.get_peripherals_with_services(&[SERVICE.parse().unwrap()])
                    central.scan();
                    self.next = Some(RootHandler::new());
                }
                _ => {}
            }
        };

        if let Some(handler) = &mut self.next {
            handler.handle_event(event, central);
        }
        // std::vec::Vec::retain is calling predicate with immutable borrow,
        // so we have to use either retain_mut crate or interior mutability (via RefCell)
        self.handlers.retain(|(f, sender)|
            if let Some(result) = f(event) {
                let mut sender = sender.borrow_mut();
                sender.blocking_send(result);
                false
            } else {
                true
            });
    }
}

struct RootHandler<'a> {
    connected_peripherals: HashSet<Peripheral>,
    peripherals: HashMap<Uuid, PeripheralInfo>,
    sender: broadcast::Sender<Arc<Mutex<CentralEvent>>>,
    receiver: broadcast::Receiver<Arc<Mutex<CentralEvent>>>,
    next: Option<DeviceHandler<'a>>,
}

impl RootHandler<'_> {
    fn new() -> Self {
        let (sender, receiver) = broadcast::channel(100);
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
        self.peripherals.iter().find_map(|(uuid, peripheral_info)|
            if uuid.to_string().contains(s) { Some(peripheral_info.clone()) } else { None })
    }

    fn get_connected_device(&self, uuid: Uuid) -> Option<Peripheral> {
        self.connected_peripherals.iter().find_map(|peripheral|
            if uuid == peripheral.id() { Some(peripheral.clone()) } else { None })
    }


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
