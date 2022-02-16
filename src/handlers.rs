use std::collections::HashMap;
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
                     uuid::Uuid};
use futures::{select, StreamExt};
use log::*;
use postage::{*, prelude::Sink};
use postage::mpsc::Sender;

// --- Implementation draft ----------------------------------------------------
// https://stackoverflow.com/questions/27957103/how-do-i-create-a-heterogeneous-collection-of-objects
// https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=308ca372ab13fdb3ecec6f4a3702895d


#[derive(Debug, Clone)]
pub enum CommandResult {
    GetStatus(Duration, Option<usize>),
    ListPeripherals(Vec<(Peripheral, AdvertisementData)>),
    FindPeripheral(Option<(Peripheral, AdvertisementData)>),
    ConnectToPeripheral(Peripheral),
    FindService(Peripheral, Option<Service>),
    FindCharacteristic(Peripheral, Option<Characteristic>),
    ReadCharacteristic(Peripheral, Characteristic, Option<Vec<u8>>),
    WriteCharacteristic(Peripheral, Characteristic, Result<(), Error>),
}

#[derive(Debug, Clone)]
pub enum Command {
    GetStatus,
    ListPeripherals,
    FindPeripheral(String),
    FindPeripheralByService(String),
    ConnectToPeripheral((Peripheral, AdvertisementData)),
    RegisterConnectedPeripheral(Peripheral),
    UnregisterConnectedPeripheral(Uuid),
    FindService(Peripheral, String),
    FindCharacteristic(Peripheral, Service, String, Uuid),
    ReadCharacteristic(Peripheral, Characteristic),
    WriteCharacteristic(Peripheral, Characteristic, Vec<u8>),
}
// OPTIMIZE(df): Move matchers to separate file?

enum EventMatchResult {
    Next(Command),
    Result(CommandResult),
    None,
}

impl EventMatchResult {
    fn is_none(&self) -> bool {
        matches!(*self, EventMatchResult::None)
    }
}

trait EventMatcher {
    // FROM_HERE(df): Result of event matching can be not only CommandResult.
    // We can send another Command, for example on event match.
    // Thus, we have to decouple sending `CommandResult` from successful event matching.
    // as an option, pass Sender to created Matcher and return boolean on successful match.
    fn matches(&self, event: &CentralEvent) -> EventMatchResult;
}

struct PeripheralDiscoveredMatcherByUUIDSubstr {
    uuid_substr: String,
}

impl PeripheralDiscoveredMatcherByUUIDSubstr {
    fn new(uuid_substr: String) -> Self {
        Self { uuid_substr }
    }
}

impl EventMatcher for PeripheralDiscoveredMatcherByUUIDSubstr {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
            if peripheral.id().to_string().contains(&self.uuid_substr) {
                info!("discovered {:?}", peripheral);
                return EventMatchResult::Result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))))
            }
        }
        EventMatchResult::None
    }
}


struct PeripheralDiscoveredMatcherByServiceUUIDSubstr {
    uuid_substr: String,
}

impl PeripheralDiscoveredMatcherByServiceUUIDSubstr {
    fn new(uuid_substr: String) -> Self {
        Self { uuid_substr }
    }
}

impl EventMatcher for PeripheralDiscoveredMatcherByServiceUUIDSubstr {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        info!("Incoming event: {:?}", event);
        if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
            info!("discovered {:?}, matching against {}", peripheral, self.uuid_substr);
            if advertisement_data.service_uuids().iter()
                .find(|uuid| uuid.to_string().contains(&self.uuid_substr)).is_some() {
                return EventMatchResult::Result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))))
            }
        }
        EventMatchResult::None
    }
}

struct PeripheralConnectedMatcher {
    peripheral_id: Uuid,
}

impl PeripheralConnectedMatcher {
    fn new(peripheral: &Peripheral) -> Self {
        Self {
            peripheral_id: peripheral.id()
        }
    }
}

impl EventMatcher for PeripheralConnectedMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::PeripheralConnected { peripheral } = event {
            if peripheral.id() == self.peripheral_id {
                info!("connected peripheral {:?}", peripheral);
                return EventMatchResult::Next(Command::RegisterConnectedPeripheral(peripheral.clone()))
            }
        } else if let CentralEvent::PeripheralConnectFailed { peripheral, error } = event {
            if peripheral.id() == self.peripheral_id {
                let id = peripheral.id().clone();
                // TODO(df): send command to unregister ad data
                error!("failed to connect to peripheral {:?}: {:?}", peripheral, error);
                return EventMatchResult::Next(Command::UnregisterConnectedPeripheral(id))
            }
        }
        EventMatchResult::None
    }
}


struct ServicesDiscoveredMatcher {
    peripheral_id: Uuid,
    uuid_substr: String,
}

impl ServicesDiscoveredMatcher {
    fn new(peripheral: &Peripheral, uuid_substr: String) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            uuid_substr,
        }
    }
}

impl EventMatcher for ServicesDiscoveredMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
            if peripheral.id() == self.peripheral_id {
                return EventMatchResult::Result(CommandResult::FindService(
                    peripheral.clone(),
                    if let Ok(services) = services {
                        services.iter()
                            .find(|s| s.id().to_string().contains(&self.uuid_substr))
                            .map(|s| s.clone())
                    } else {
                        None
                    }))
            }
        }
        EventMatchResult::None
    }
}


struct CharacteristicsDiscoveredMatcher {
    peripheral_id: Uuid,
    service_id: Uuid,
    uuid_substr: String,
}

impl CharacteristicsDiscoveredMatcher {
    fn new(peripheral: &Peripheral, service: &Service, uuid_substr: String) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            service_id: service.id(),
            uuid_substr,
        }
    }
}

impl EventMatcher for CharacteristicsDiscoveredMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
            if peripheral.id() == self.peripheral_id && service.id() == self.service_id {
                return EventMatchResult::Result(CommandResult::FindCharacteristic(
                    peripheral.clone(),
                    if let Ok(characteristics) = characteristics {
                        characteristics.iter()
                            .find(|s| s.id().to_string().contains(&self.uuid_substr))
                            .map(|s| s.clone())
                    } else {
                        None
                    }))
            }
        }
        EventMatchResult::None
    }
}

struct CharacteristicValueMatcher {
    peripheral_id: Uuid,
    characteristic_id: Uuid,
}

impl CharacteristicValueMatcher {
    fn new(peripheral: &Peripheral, characteristic: &Characteristic) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
        }
    }
}

impl EventMatcher for CharacteristicValueMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return EventMatchResult::Result(CommandResult::ReadCharacteristic(
                    peripheral.clone(),
                    characteristic.clone(),
                    if let Ok(value) = value { Some(value.clone()) } else { None }))
            }
        }
        EventMatchResult::None
    }
}

struct WriteCharacteristicResultMatcher {
    peripheral_id: Uuid,
    characteristic_id: Uuid,
}

impl WriteCharacteristicResultMatcher {
    fn new(peripheral: &Peripheral, characteristic: &Characteristic) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
        }
    }
}

impl EventMatcher for WriteCharacteristicResultMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatchResult {
        if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return EventMatchResult::Result(
                    CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone()));
            }
        }
        EventMatchResult::None
    }
}

pub struct Controller {
    sender: mpsc::Sender<(Command, oneshot::Sender<CommandResult>)>,
}

impl Controller {
    pub async fn execute(&mut self, command: Command) -> CommandResult {
        let (sender, mut receiver) = oneshot::channel();
        self.sender.send((command, sender)).await.ok();
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
        let command_sender_copy = command_sender.clone();
        (Self {
            handle: task::spawn(
                async move {
                    let (central, central_receiver) = CentralManager::new();
                    let mut handler = InnerHandler {
                        started_at: Instant::now(),
                        state: ManagerState::Unknown,
                        command_sender,
                        matchers: vec![],
                        connected_peripherals: Default::default(),
                        peripheral_ads: Default::default(),
                    };
                    let mut command_receiver = command_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();
                    // Events handler loop
                    loop {
                        select! {

                            // FROM_HERE(df):
                            // Current execution flow for async commands:
                            // in handler.execute:
                            // change struct state via &mut self -> add matcher -> matcher sends CommandResult to the channel -> CommandResult is returned to caller
                            // the problem is we may want to process CommandResult again and issue a new command before returning result

                            // `execute` can't return CommandResult directly, it has to be sent to caller via oneshot channel
                            // so we have to tweak matcher

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
         Controller { sender: command_sender_copy })
    }
}

type MatcherPair = (Box<dyn Fn(&CentralEvent) -> EventMatchResult + Send>, oneshot::Sender<CommandResult>);

struct InnerHandler {
    started_at: Instant,
    state: ManagerState,
    command_sender: Sender<(Command, oneshot::Sender<CommandResult>)>,
    matchers: Vec<MatcherPair>,
    connected_peripherals: HashMap<Uuid, Peripheral>,
    peripheral_ads: HashMap<Uuid, AdvertisementData>,
}

impl InnerHandler {
    fn execute(&mut self, command: Command, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager) {
        match command {
            Command::GetStatus => {
                sender.blocking_send(CommandResult::GetStatus(
                    Duration::new(self.started_at.elapsed().as_secs(), 0),
                    Some(self.connected_peripherals.len()), //TODO(df): Update status output
                )).unwrap()
            }
            Command::FindPeripheral(uuid_substr) => {
                info!("Finding peripheral with substring '{:?}'", uuid_substr);
                if let Some(result) = self.find_connected_peripheral(uuid_substr.clone()) {
                    info!("Found connected peripheral '{:?}'", result.0);
                    sender.blocking_send(CommandResult::FindPeripheral(Some(result))).unwrap();
                } else {
                    // TODO(df): Add timeout and Ctrl+C handling
                    self.add_matcher(sender, PeripheralDiscoveredMatcherByUUIDSubstr::new(uuid_substr));
                }
            }
            Command::FindPeripheralByService(uuid_substr) => {
                info!("Finding peripheral with service '{:?}'", uuid_substr);
                if let Some(result) = self.find_connected_peripheral_by_service(uuid_substr.clone()) {
                    info!("Found connected peripheral '{:?}'", result.0);
                    sender.blocking_send(CommandResult::FindPeripheral(Some(result))).unwrap();
                } else {
                    // TODO(df): Add timeout and Ctrl+C handling
                    self.add_matcher(sender, PeripheralDiscoveredMatcherByServiceUUIDSubstr::new(uuid_substr));
                }
            }
            Command::ConnectToPeripheral((peripheral, advertisement_data)) => {
                let id = peripheral.id().clone();

                // TODO(df): Do we need to look up in connected peripherals?
                if let Some(result) = self.get_connected_peripheral(id) {
                    sender.blocking_send(CommandResult::ConnectToPeripheral(result.0)).unwrap();
                } else {
                    self.peripheral_ads.insert(id, advertisement_data.clone());
                    self.add_matcher(sender, PeripheralConnectedMatcher::new(&peripheral));
                    central.connect(&peripheral);
                }
            }
            Command::RegisterConnectedPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                info!("registered peripheral {}, total {}", id, self.connected_peripherals.len());
                self.connected_peripherals.insert(id, peripheral.clone());
            }
            Command::UnregisterConnectedPeripheral(uuid) => {
                self.connected_peripherals.remove(&uuid);
                self.peripheral_ads.remove(&uuid);
            }
            Command::FindService(peripheral, uuid_substr) => {
                self.add_matcher(sender, ServicesDiscoveredMatcher::new(&peripheral, uuid_substr));
                peripheral.discover_services();
            }
            Command::FindCharacteristic(peripheral, service, uuid_substr, _) => {
                self.add_matcher(sender, CharacteristicsDiscoveredMatcher::new(&peripheral, &service, uuid_substr));
                peripheral.discover_characteristics(&service);
            }
            Command::ReadCharacteristic(peripheral, characteristic) => {
                self.add_matcher(sender, CharacteristicValueMatcher::new(&peripheral, &characteristic));
                peripheral.read_characteristic(&characteristic);
            }
            Command::WriteCharacteristic(peripheral, characteristic, data) => {
                self.add_matcher(sender, WriteCharacteristicResultMatcher::new(&peripheral, &characteristic));
                peripheral.write_characteristic(&characteristic, &data, WriteKind::WithResponse);
            }
            Command::ListPeripherals => sender.blocking_send(CommandResult::ListPeripherals({
                self.connected_peripherals.values().into_iter()
                    .map(|p| return (p.clone(), self.peripheral_ads.get(&p.id()).unwrap().clone()))
                    .collect()
            })).unwrap()
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
                        //TODO(df): Run different type of peripherals search depending on params

                        // for example, by service uuid:
                        // central.scan_with_options(ScanOptions::default().allow_duplicates(false));

                        // this will not work on MacOSX 12.1, see https://stackoverflow.com/a/70657368/1016019
                        central.scan();
                    }
                    _ => {}
                }
            } /*
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
            } */
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
            CentralEvent::PeripheralDisconnected {
                peripheral,
                error: _,
            } => {
                self.connected_peripherals.remove(&peripheral.id());
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
        let mut command_sender = self.command_sender.clone();
        let mut i = 0;
        info!("handle {:?} event for {} matchers", event, self.matchers.len());
        while i < self.matchers.len() {
            info!("handle_event: {:?}", event);
            let result = self.matchers[i].0(event);
            if result.is_none() {
                i += 1;
            } else {
                let (_, mut sender) = self.matchers.remove(i);
                info!("removing matcher #{}", i);
                match result {
                    EventMatchResult::Next(command) => {
                        command_sender.try_send((command, sender)).ok();
                    }
                    EventMatchResult::Result(result) => {
                        sender.blocking_send(result).unwrap();
                    }
                    EventMatchResult::None => {}
                }
            }
        }

        info!("event handling complete, {} matchers left", self.matchers.len());
    }

    fn add_matcher(&mut self, sender: oneshot::Sender<CommandResult>, matcher: impl EventMatcher + Send + 'static) {
        let item: MatcherPair = (Box::new(move |event| matcher.matches(event)), sender);
        info!("adding matcher" );
        self.matchers.push(item);
    }

    fn find_connected_peripheral(&mut self, uuid_substr: String) -> Option<(Peripheral, AdvertisementData)> {
        let s = uuid_substr.as_str();
        if let Some(peripheral) = self.connected_peripherals.values().find(|peripheral|
            peripheral.id().to_string().contains(s)) {
            Some((peripheral.clone(), self.peripheral_ads.get(&peripheral.id()).unwrap().clone()))
        } else {
            None
        }
    }

    fn find_connected_peripheral_by_service(&self, uuid_substr: String) -> Option<(Peripheral, AdvertisementData)> {
        let s = uuid_substr.as_str();
        self.peripheral_ads.iter().find(|(_, ad)|
            ad.service_uuids().iter().find(|service_uuid| service_uuid.to_string().contains(s)).is_some())
            .and_then(|(peripheral_uuid, ad)| {
                Some((self.connected_peripherals.get(peripheral_uuid).unwrap().clone(), ad.clone()))
            })
    }

    fn get_connected_peripheral(&self, uuid: Uuid) -> Option<(Peripheral, AdvertisementData)> {
        if let Some(peripheral) = self.connected_peripherals.get(&uuid) {
            Some((peripheral.clone(), self.peripheral_ads.get(&peripheral.id()).unwrap().clone()))
        } else {
            None
        }
    }
}
