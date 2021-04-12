use log::*;

use std::collections::{HashMap, HashSet};
use std::process::exit;

use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::*;
use core_bluetooth::uuid::Uuid;
use core_bluetooth::*;

use crate::Position;
use core::fmt;
use std::time::{Instant, Duration};
use core_bluetooth::central::service::Service;
use async_std::sync::Arc;
use std::fmt::{Debug, Formatter};

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

#[derive(Debug)]
pub struct PeripheralInfo {
    peripheral: Peripheral,
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

pub enum HandlerCommand<T> {
    ListDevices(Box<dyn FnOnce(&HashMap<Uuid, PeripheralInfo>) -> T + Send + 'static>),
    GetStatus(Box<dyn FnOnce(Duration, Option<usize>) -> T + Send + 'static>),
    FindMatch(String, Box<dyn FnOnce(Option<String>) -> T + Send + 'static>),
}

impl<T> HandlerCommand<T> {
    fn name(&self) -> &'static str {
        use HandlerCommand::*;
        match self {
            ListDevices(_) => "ListDevices",
            GetStatus(_) => "GetStatus",
            FindMatch(_, _) => "FindMatch"
        }
    }
}

pub struct InitHandler<'a> {
    started_at: Instant,
    state: ManagerState,
    next: Option<RootHandler<'a>>,
}

impl InitHandler<'_>
{
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            state: ManagerState::Unknown,
            next: None,
        }
    }

    pub fn execute<T>(&mut self, command: HandlerCommand<T>) -> T {
        match command {
            HandlerCommand::GetStatus(callback) => {
                callback(
                    Duration::new(self.started_at.elapsed().as_secs(), 0),
                    if let Some(handler) = &self.next {
                        Some(handler.peripherals.len())
                    } else {
                        None
                    },
                )
            }
            _ => {
                if let Some(handler) = &mut self.next {
                    handler.execute(command)
                } else {
                    panic!("Can't execute command '{}'", command.name());
                }
            }
        }
    }

    pub fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        if let CentralEvent::ManagerStateChanged { new_state } = event {
            println!("New state event: {:?}", event);
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
                    // match &self.peripheral_uuid {
                    //    Some(uuid) => central.get_peripherals(&[uuid.parse().unwrap()]),
                    //    None => central.scan(),
                    //}
                    central.scan();
                    self.next = Some(RootHandler::new());
                    //TODO(df): Run different type of peripherals search depending on params
                    //central.get_peripherals_with_services(&[SERVICE.parse().unwrap()]) // TODO(df): Implement connection by service uuid
                    //central.scan();
                }
                _ => {}
            }
        } else if let Some(handler) = &mut self.next {
            handler.handle_event(event, central);
        }
    }
}

struct RootHandler<'a> {
    connected_peripherals: HashSet<Peripheral>,
    peripherals: HashMap<Uuid, PeripheralInfo>,
    next: Option<DeviceHandler<'a>>,
}

impl RootHandler<'_> {
    fn new() -> Self {
        Self {
            connected_peripherals: HashSet::new(),
            peripherals: HashMap::new(),
            next: None,
        }
    }

    fn execute<T>(&mut self, command: HandlerCommand<T>) -> T {
        match command {
            HandlerCommand::ListDevices(callback) => {
                callback(&self.peripherals)
            }
            HandlerCommand::FindMatch(substring, callback) => {
                callback(
                    if let Some(handler) = &mut self.next {
                        None // TODO(df): Add matching for next
                    } else {
                        let s = substring.as_str();
                        let mut result = None;
                        for (uuid, peripheral) in &self.peripherals {
                            let uuid_string = uuid.to_string();
                            if uuid_string.contains(s) {
                                result = Some(uuid_string);
                                break;
                            }
                        }
                        result
                    })
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

    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        match event {
            CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                rssi: _,
            } => {
                println!("[PeripheralDiscovered]: {}", self.peripherals.len());
                self.peripherals.insert(
                    peripheral.id(),
                    PeripheralInfo {
                        peripheral: peripheral.clone(),
                        advertisement_data: advertisement_data.clone(),
                    },
                );

                /* if advertisement_data.is_connectable() != Some(false) &&
                    self.connected_peripherals.insert(peripheral.clone())
                {
                    info!("connecting to {} {} dB ({:?})",
                          peripheral.id(), rssi, advertisement_data.local_name());
                    self.central.connect(&peripheral);
                }*/
            }

            CentralEvent::GetPeripheralsResult {
                peripherals,
                tag: _,
            } => {
                for peripheral in peripherals {
                    println!("[GetPeripheralsResult]: {}", peripheral.id());

                    //  if let Some(d) = data.get(&TypeId::of::<ConnectionHandler>()).unwrap().downcast_ref::<RefCell<ConnectionHandler>>() {
                    //     println!("entry found: {:#?}", d.deref().borrow().connection_state);
                    // }
                    /* let _: &ConnectionHandler = match data.get("connection").deref().borrow().downcast_ref::<ConnectionHandler>() {
                                             Some(b) => b,
                                             None => panic!("isn't a ConnectionHandler!")
                                         };
                    */
                    if self.connected_peripherals.insert(peripheral.clone()) {
                        //println!("connecting to {})", peripheral.id());
                        central.connect(&peripheral);
                    }
                }
            }
            CentralEvent::PeripheralConnected { peripheral } => {
                peripheral.discover_services_with_uuids(&[SERVICE.parse().unwrap()]);
            }
            CentralEvent::PeripheralDisconnected {
                peripheral,
                error: _,
            } => {
                self.connected_peripherals.remove(&peripheral);
                //println!("re-connecting to {})", peripheral.id());
                central.connect(&peripheral);
            }
            CentralEvent::GetPeripheralsWithServicesResult {
                peripherals,
                tag: _,
            } => {
                println!("[GetPeripheralsWithServicesResult]: {}", peripherals.len());
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

    fn execute<T>(&self, command: HandlerCommand<T>) -> T {
        panic!("Can't execute command {}", command.name());
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
