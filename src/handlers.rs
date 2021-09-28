use core::fmt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::process::exit;
use std::time::{Duration, Instant};

use async_std::{task, task::JoinHandle};
use core_bluetooth::{central::{*,
                               characteristic::{Characteristic, WriteKind},
                               peripheral::Peripheral,
                               service::Service},
                     error::Error,
                     ManagerState,
                     uuid::Uuid as BLE_Uuid};
use futures::{select, StreamExt};
use log::*;
use postage::{*, prelude::Sink};
use uuid::Uuid;

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

        if f.precision().unwrap_or(0) == 0 {
            write!(
                f,
                "[{}] {}",
                self.peripheral.id(),
                display_name)
        } else {
            write!(
                f,
                "[{}] {}: {} services",
                self.peripheral.id(),
                display_name,
                self.advertisement_data.service_uuids().len())
        }
    }
}

#[derive(Debug, Clone)]
pub enum CommandResult {
    GetStatus(Duration, Option<(usize, usize)>),
    ListConnectedPeripherals(HashMap<BLE_Uuid, PeripheralInfo>),
    ListPeripherals(HashMap<BLE_Uuid, PeripheralInfo>),
    FindPeripheral(Option<PeripheralInfo>),
    ConnectToPeripheral(PeripheralInfo),
    FindService(Peripheral, Option<Service>),
    FindCharacteristic(Peripheral, Option<Characteristic>),
    ReadCharacteristic(Peripheral, Characteristic, Option<Vec<u8>>),
    WriteCharacteristic(Peripheral, Characteristic, Result<(), Error>),
}

#[derive(Debug, Clone)]
pub enum Command {
    GetStatus,
    ListPeripherals,
    ListConnectedPeripherals,
    FindPeripheral(String),
    FindPeripheralByService(BLE_Uuid),
    ConnectToPeripheral(Peripheral),
    FindService(Peripheral, String),
    FindCharacteristic(Peripheral, Service, String, Uuid),
    ReadCharacteristic(Peripheral, Characteristic),
    WriteCharacteristic(Peripheral, Characteristic, Vec<u8>),
}
// TODO(df): Move to separate file?

trait EventMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult>;
}


struct PeripheralDiscoveredMatcher {
    peripheral_id: BLE_Uuid,
    handler: Box<dyn Fn(&Peripheral) -> CommandResult + Send + 'static>,
}

impl PeripheralDiscoveredMatcher {
    fn new(peripheral: &Peripheral, handler: impl Fn(&Peripheral) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for PeripheralDiscoveredMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        if let CentralEvent::PeripheralConnected { peripheral } = event {
            if peripheral.id() == self.peripheral_id {
                info!("connected peripheral {:?}", peripheral);
                return Some((&self.handler)(&peripheral));
            }
        }
        None
    }
}


struct PeripheralConnectedMatcher {
    peripheral_id: BLE_Uuid,
    handler: Box<dyn Fn(&Peripheral) -> CommandResult + Send + 'static>,
}

impl PeripheralConnectedMatcher {
    fn new(peripheral: &Peripheral, handler: impl Fn(&Peripheral) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for PeripheralConnectedMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        //TODO(df): handle CentralEvent::PeripheralConnectFailed { peripheral, error }
        if let CentralEvent::PeripheralConnected { peripheral } = event {
            if peripheral.id() == self.peripheral_id {
                info!("connected peripheral {:?}", peripheral);
                return Some((&self.handler)(&peripheral));
            }
        }
        None
    }
}

struct ServicesDiscoveredMatcher {
    peripheral_id: BLE_Uuid,
    uuid_substr: String,
    handler: Box<dyn Fn(&Peripheral, Option<Service>) -> CommandResult + Send + 'static>,
}

impl ServicesDiscoveredMatcher {
    fn new(peripheral: &Peripheral, uuid_substr: String,
           handler: impl Fn(&Peripheral, Option<Service>) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            uuid_substr,
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for ServicesDiscoveredMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
            if peripheral.id() == self.peripheral_id {
                return Some((&self.handler)
                    (&peripheral,
                     if let Ok(services) = services {
                         services.iter()
                             .find(|s| s.id().to_string().contains(&self.uuid_substr))
                             .map(|s| s.clone())
                     } else {
                         None
                     }));
            }
        }
        None
    }
}

struct CharacteristicValueMatcher {
    peripheral_id: BLE_Uuid,
    characteristic_id: BLE_Uuid,
    handler: Box<dyn Fn(&Peripheral, &Characteristic, &Result<Vec<u8>, Error>) -> CommandResult + Send + 'static>,
}

impl CharacteristicValueMatcher {
    fn new(peripheral: &Peripheral, characteristic: &Characteristic,
           handler: impl Fn(&Peripheral, &Characteristic, &Result<Vec<u8>, Error>) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for CharacteristicValueMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return Some((&self.handler)(peripheral, characteristic, value));
            }
        }
        None
    }
}


struct CharacteristicsDiscoveredMatcher {
    peripheral_id: BLE_Uuid,
    service_id: BLE_Uuid,
    uuid_substr: String,
    handler: Box<dyn Fn(&Peripheral, Option<Characteristic>) -> CommandResult + Send + 'static>,
}

impl CharacteristicsDiscoveredMatcher {
    fn new(peripheral: &Peripheral, service: &Service, uuid_substr: String,
           handler: impl Fn(&Peripheral, Option<Characteristic>) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            service_id: service.id(),
            uuid_substr,
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for CharacteristicsDiscoveredMatcher {
    //noinspection DuplicatedCode
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
            if peripheral.id() == self.peripheral_id && service.id() == self.service_id {
                return Some((&self.handler)
                    (peripheral,
                     if let Ok(characteristics) = characteristics {
                         characteristics.iter()
                             .find(|s| s.id().to_string().contains(&self.uuid_substr))
                             .map(|s| s.clone())
                     } else {
                         None
                     },
                    ));
            }
        }
        None
    }
}


struct WriteCharacteristicResultMatcher {
    peripheral_id: BLE_Uuid,
    characteristic_id: BLE_Uuid,
    handler: Box<dyn Fn(&Peripheral, &Characteristic, &Result<(), Error>) -> CommandResult + Send + 'static>,
}

impl WriteCharacteristicResultMatcher {
    fn new(peripheral: &Peripheral, characteristic: &Characteristic,
           handler: impl Fn(&Peripheral, &Characteristic, &Result<(), Error>) -> CommandResult + Send + 'static) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
            handler: Box::new(handler),
        }
    }
}

impl EventMatcher for WriteCharacteristicResultMatcher {
    fn match_event(&self, event: &CentralEvent) -> Option<CommandResult> {
        if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return Some((&self.handler)(peripheral, characteristic, result));
            }
        }
        None
    }
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

pub struct AsyncManager {
    handle: JoinHandle<()>,
}

impl AsyncManager {
    pub fn new() -> (Self, Controller) {
        let (command_sender, command_receiver) = mpsc::channel::<(Command, oneshot::Sender<CommandResult>)>(100);
        (Self {
            handle: task::spawn(
                async move {
                    let (central, central_receiver) = CentralManager::new();
                    let mut handler = InnerHandler {
                        started_at: Instant::now(),
                        state: ManagerState::Unknown,
                        handlers: vec![],
                        connected_peripherals: Default::default(),
                        peripherals: Default::default(),
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

struct InnerHandler {
    started_at: Instant,
    state: ManagerState,
    handlers: Vec<(Box<dyn Fn(&CentralEvent) -> Option<CommandResult> + Send>, RefCell<oneshot::Sender<CommandResult>>)>,
    connected_peripherals: HashSet<Peripheral>,
    peripherals: HashMap<BLE_Uuid, PeripheralInfo>,
}

impl InnerHandler {
    fn execute(&mut self, command: Command, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager) {
        match command {
            Command::GetStatus => {
                sender.blocking_send(CommandResult::GetStatus(
                    Duration::new(self.started_at.elapsed().as_secs(), 0),
                    Some((self.peripherals.len(), self.connected_peripherals.len())),
                )).unwrap()
            }
            Command::ListConnectedPeripherals => sender.blocking_send(CommandResult::ListConnectedPeripherals({
                let mut connected = self.peripherals.clone();
                connected.retain(|_k, v| { self.connected_peripherals.contains(&v.peripheral) });
                connected
            })).unwrap(),
            Command::ListPeripherals => sender.blocking_send(CommandResult::ListPeripherals(
                self.peripherals.clone()
            )).unwrap(),
            Command::FindPeripheral(uuid_substr) => {
                info!("Finding peripheral with substring '{:?}'", uuid_substr);
                let peripheral = self.find_peripheral(uuid_substr);
                info!("Peripherals search result '{:?}'", peripheral);
                sender.blocking_send(CommandResult::FindPeripheral(peripheral)).unwrap()
            }
            Command::FindPeripheralByService(uuid) => {
                info!("Finding peripheral with service '{:?}'", uuid);
                let peripheral = self.find_peripheral_by_service(uuid);
                info!("Peripherals search result '{:?}'", peripheral);
                sender.blocking_send(CommandResult::FindPeripheral(peripheral)).unwrap()
            }
            Command::ConnectToPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                let info = self.peripherals.get(&id).unwrap().clone();

                if let Some(_peripheral) = self.get_connected_peripheral(id) {
                    sender.blocking_send(CommandResult::ConnectToPeripheral(info.clone())).unwrap();
                } else {
                    &self.add_matcher(sender, PeripheralConnectedMatcher::new(
                        &peripheral,
                        move |_peripheral| {
                            return CommandResult::ConnectToPeripheral(info.clone());
                        }));
                    central.connect(&peripheral);
                }
            }
            Command::FindService(peripheral, uuid_substr) => {
                &self.add_matcher(sender, ServicesDiscoveredMatcher::new(
                    &peripheral, uuid_substr,
                    |peripheral, service| {
                        CommandResult::FindService(peripheral.clone(), service)
                    }));
                peripheral.discover_services();
            }
            Command::FindCharacteristic(peripheral, service, uuid_substr, _) => {
                &self.add_matcher(sender, CharacteristicsDiscoveredMatcher::new(
                    &peripheral, &service, uuid_substr,
                    move |peripheral, characteristic| {
                        CommandResult::FindCharacteristic(peripheral.clone(), characteristic)
                    }));
                peripheral.discover_characteristics(&service);
            }
            Command::ReadCharacteristic(peripheral, characteristic) => {
                &self.add_matcher(sender, CharacteristicValueMatcher::new(
                    &peripheral, &characteristic,
                    |peripheral, characteristic, value| {
                        CommandResult::ReadCharacteristic(
                            peripheral.clone(),
                            characteristic.clone(),
                            if let Ok(value) = value { Some(value.clone()) } else { None })
                    }));
                peripheral.read_characteristic(&characteristic);
            }
            Command::WriteCharacteristic(peripheral, characteristic, data) => {
                &self.add_matcher(sender, WriteCharacteristicResultMatcher::new(
                    &peripheral, &characteristic, |peripheral, characteristic, result| {
                        CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone())
                    },
                ));
                peripheral.write_characteristic(&characteristic, &data, WriteKind::WithResponse);
            }
        };
    }


    fn handle_event(&mut self, event: &CentralEvent, central: &CentralManager) {
        match event {
            CentralEvent::ManagerStateChanged { new_state } => {
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
                        // central.scan_with_options(ScanOptions::default().allow_duplicates(false));
                        central.scan();
                    }
                    _ => {}
                }
            }
            CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                rssi: _,
            } => {
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
                /*for peripheral in peripherals {
                    println!("Discovered with result: {}", peripheral.id());
                        if self.connected_peripherals.insert(p.clone()) {
                        debug!("connecting to {})", p.id());
                        self.central.connect(&p);
                    }
                } */
            }

            _ => {}
        }

        // std::vec::Vec::retain is calling predicate with immutable borrow,
        // so we have to use either retain_mut crate or interior mutability (via RefCell)
        self.handlers.retain(|(f, sender)|
            if let Some(result) = f(event) {
                let mut sender = sender.borrow_mut();
                sender.blocking_send(result).unwrap();
                false
            } else {
                true
            });
    }

    fn add_matcher(&mut self, sender: oneshot::Sender<CommandResult>, matcher: impl EventMatcher + Send + 'static) {
        &self.handlers.push((Box::new(move |event| matcher.match_event(event)), RefCell::new(sender)));
    }

    fn find_peripheral(&self, uuid_substr: String) -> Option<PeripheralInfo> {
        let s = uuid_substr.as_str();
        self.peripherals.iter().find_map(|(uuid, peripheral_info)|
            if uuid.to_string().contains(s) { Some(peripheral_info.clone()) } else { None })
    }

    fn find_peripheral_by_service(&self, uuid: BLE_Uuid) -> Option<PeripheralInfo> {
        self.peripherals.iter().find_map(|(_, peripheral_info)|
            if peripheral_info.advertisement_data.service_uuids().contains(&uuid)
            { Some(peripheral_info.clone()) } else { None }
        )
    }

    fn get_connected_peripheral(&self, uuid: BLE_Uuid) -> Option<Peripheral> {
        self.connected_peripherals.iter().find_map(|peripheral|
            if uuid == peripheral.id() { Some(peripheral.clone()) } else { None })
    }
}