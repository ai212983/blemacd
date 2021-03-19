use log::*;

use std::collections::{HashMap, HashSet};
use std::process::exit;

use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::*;
use core_bluetooth::uuid::Uuid;
use core_bluetooth::*;

use crate::Position;
use core::fmt;

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

pub enum HandlerOperation {
    None,
    Remove(Position),
    Insert(Position, Option<EventHandler>),
}

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

pub trait Handler: 'static {
    // TODO(df): Do we need lifetime here?
    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) -> HandlerOperation;
    fn position(&self) -> Position;
}

pub struct ConnectionHandler {
    pub peripheral_uuid: Option<String>,
    pub status: ManagerState,
}

impl ConnectionHandler {
    pub fn new() -> Self {
        Self {
            peripheral_uuid: None,
            status: ManagerState::Unknown,
        }
    }

    pub fn get_status(&self) -> ManagerState {
        return self.status;
    }
}

impl Handler for ConnectionHandler {
    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) -> HandlerOperation {
        if let CentralEvent::ManagerStateChanged { new_state } = event {
            self.status = *new_state;
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
                    return HandlerOperation::Remove(Position::Devices);
                    // TODO(df): Clean up handlers
                }
                ManagerState::PoweredOn => {
                    //info!("bt is powered on, starting peripherals scan");
                    match &self.peripheral_uuid {
                        Some(uuid) => central.get_peripherals(&[uuid.parse().unwrap()]),
                        None => central.scan(),
                    }
                    return HandlerOperation::Insert(
                        Position::Devices,
                        Option::Some(EventHandler::Peripherals(PeripheralsHandler {
                            connected_peripherals: HashSet::new(),
                            peripherals: HashMap::new(),
                        })),
                    );
                    //TODO(df): Run different type of peripherals search depending on params
                    //central.get_peripherals_with_services(&[SERVICE.parse().unwrap()]) // TODO(df): Implement connection by service uuid
                    //central.scan();
                }
                _ => {}
            }
        }
        HandlerOperation::None
    }

    fn position(&self) -> Position {
        Position::Root
    }
}

pub struct PeripheralsHandler {
    pub connected_peripherals: HashSet<Peripheral>,
    pub peripherals: HashMap<Uuid, PeripheralInfo>,
}

//TODO(df): We can use different type of events (list all, get by UID)
impl Handler for PeripheralsHandler {
    //, peripherals: &mut HashSet<PeripheralInfo>
    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) -> HandlerOperation {
        //println!("Peripherals event {:?}", event);
        match event {
            CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                rssi: _,
            } => {
                //println!("Discovered by UIDs result: {}", self.connected_peripherals.len());
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
                    //println!("Discovered by UIDs result: {}", peripheral.id());

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
                //println!("Discovered by UIDs result B: {}", peripherals.len());
                for peripheral in peripherals {
                    //println!("Discovered with result: {}", peripheral.id());
                    /*    if self.connected_peripherals.insert(p.clone()) {
                        debug!("connecting to {})", p.id());
                        self.central.connect(&p);
                    }*/
                }
            }
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
        HandlerOperation::None
    }

    fn position(&self) -> Position {
        Position::Devices
    }
}

//

// Using enum wrappers for series of trait implementations,
// see https://bennetthardwick.com/blog/dont-use-boxed-trait-objects-for-struct-internals/

pub enum EventHandler {
    Connection(ConnectionHandler),
    Peripherals(PeripheralsHandler),
}

impl Handler for EventHandler {
    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) -> HandlerOperation {
        match self {
            EventHandler::Connection(handler) => handler.handle_event(event, central),
            EventHandler::Peripherals(handler) => handler.handle_event(event, central),
        }
    }

    fn position(&self) -> Position {
        match self {
            EventHandler::Connection(handler) => handler.position(),
            EventHandler::Peripherals(handler) => handler.position(),
        }
    }
}
