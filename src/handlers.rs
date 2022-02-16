use std::collections::HashMap;
use std::process::exit;
use std::time::{Duration, Instant};

use async_std::{task, task::JoinHandle};
use core_bluetooth::{central::{*,
                               characteristic::WriteKind,
                               peripheral::Peripheral},
                     ManagerState,
                     uuid::Uuid};
use futures::{select, StreamExt};
use log::*;
use postage::{*, prelude::Sink};
use postage::mpsc::Sender;

use crate::commands::*;
use crate::event_matchers::*;

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
            panic!("unexpected 'None' result, sender was prematurely dropped?");
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
                        peripherals: Default::default(),
                        advertisements: Default::default(),
                    };
                    let mut command_receiver = command_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();
                    // Events handler loop
                    loop {
                        select! {
                            command = command_receiver.next() => if let Some((command, sender)) = command {
                                handler.execute(command, sender, &central)
                            },
                            central_event = central_receiver.next() => if let Some(event) = central_event {
                                 handler.handle_event(&event);
                            }
                        }
                    }
                })
        },
         Controller { sender: command_sender_copy })
    }
}

type MatcherPair = (Box<dyn Fn(&CentralEvent) -> EventMatch + Send>, oneshot::Sender<CommandResult>);

struct InnerHandler {
    started_at: Instant,
    state: ManagerState,
    command_sender: Sender<(Command, oneshot::Sender<CommandResult>)>,
    matchers: Vec<MatcherPair>,
    peripherals: HashMap<Uuid, Peripheral>,
    advertisements: HashMap<Uuid, AdvertisementData>,
}

impl InnerHandler {
    fn execute(&mut self, command: Command, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager) {
        match command {
            Command::GetStatus => {
                sender.blocking_send(CommandResult::GetStatus(
                    Duration::new(self.started_at.elapsed().as_secs(), 0),
                    Some(self.peripherals.len()), //TODO(df): Update status output
                )).unwrap()
            }
            Command::FindPeripheralByService(uuid) => {
                info!("looking for peripheral with service [{}]", uuid);
                if let Some(result) = self.find_connected_peripheral_by_service(uuid.clone()) {
                    info!("found already connected peripheral [{}]", result.0.id());
                    central.cancel_scan();
                    sender.blocking_send(CommandResult::FindPeripheral(Some(result))).unwrap();
                } else {
                    info!("no matching peripheral found, starting scan");
                    // TODO(df): Add timeout and Ctrl+C handling
                    self.add_matcher(sender, PeripheralDiscoveredMatcherByServiceUUID::new(uuid.clone()));
                    central.scan_with_options(ScanOptions::default().include_services(&vec![uuid]));
                }
            }
            Command::ConnectToPeripheral((peripheral, advertisement_data)) => {
                let id = peripheral.id().clone();
                if let Some(result) = self.get_peripheral(id) {
                    sender.blocking_send(CommandResult::ConnectToPeripheral(result.0)).unwrap();
                } else {
                    self.advertisements.insert(id, advertisement_data.clone());
                    self.add_matcher(sender, PeripheralConnectedMatcher::new(&peripheral));
                    central.connect(&peripheral);
                }
            }
            Command::RegisterConnectedPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                self.peripherals.insert(id, peripheral.clone());
                info!("peripheral [{}]{} registered, total {}", id,
                    self.advertisements.get(&id).and_then(|ad| ad.local_name()).map_or(String::new(), |s| format!(" ({})", s)),
                    self.peripherals.len());
                sender.blocking_send(CommandResult::ConnectToPeripheral(peripheral)).unwrap();
            }
            Command::UnregisterConnectedPeripheral(uuid) => {
                self.peripherals.remove(&uuid);
                self.advertisements.remove(&uuid);
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
                self.peripherals.values().into_iter()
                    .map(|p| return (p.clone(), self.advertisements.get(&p.id()).unwrap().clone()))
                    .collect()
            })).unwrap()
        };
    }

    fn handle_event(&mut self, event: &CentralEvent) {
        match event {
            CentralEvent::ManagerStateChanged { new_state } => {
                self.state = *new_state;
                match new_state {
                    ManagerState::Unsupported => {
                        eprintln!("Bluetooth is not supported on this system");
                        exit(1);
                    }
                    ManagerState::Unauthorized => {
                        eprintln!("not authorized to use Bluetooth on this system");
                        exit(1);
                    }
                    ManagerState::PoweredOff => {
                        eprintln!("Bluetooth is disabled, please enable it");
                        // TODO(df): Clean up child handler
                    }
                    ManagerState::PoweredOn => {
                        info!("Bluetooth is powered on, ready for processing");

                        // Scanning without specifying service UUIDs will not work on MacOSX 12.1
                        // see https://stackoverflow.com/a/70657368/1016019
                        // central.scan();
                    }
                    _ => {}
                }
            }
            CentralEvent::PeripheralDisconnected {
                peripheral,
                error: _,
            } => {
                self.peripherals.remove(&peripheral.id());
                info!("unregistered peripheral {}, total {}", peripheral.id(), self.peripherals.len());
            }
            _ => {}
        }
        let mut command_sender = self.command_sender.clone();
        let mut i = 0;
        while i < self.matchers.len() {
            let result = self.matchers[i].0(event);
            if result.is_none() {
                i += 1;
            } else {
                let (_, mut sender) = self.matchers.remove(i);
                match result {
                    EventMatch::Next(command) => {
                        command_sender.try_send((command, sender)).ok();
                    }
                    EventMatch::Result(result) => {
                        sender.blocking_send(result).unwrap();
                    }
                    EventMatch::None => {}
                }
            }
        }
    }

    fn add_matcher(&mut self, sender: oneshot::Sender<CommandResult>, matcher: impl EventMatcher + Send + 'static) {
        self.matchers.push((Box::new(move |event| matcher.matches(event)), sender));
    }

    fn find_connected_peripheral_by_service(&self, uuid: Uuid) -> Option<(Peripheral, AdvertisementData)> {
        self.advertisements.iter().find(|(_, ad)|
            ad.service_uuids().iter().find(|&service_uuid| service_uuid.eq(&uuid)).is_some())
            .and_then(|(peripheral_uuid, ad)| {
                Some((self.peripherals.get(peripheral_uuid).unwrap().clone(), ad.clone()))
            })
    }

    fn get_peripheral(&self, uuid: Uuid) -> Option<(Peripheral, AdvertisementData)> {
        if let Some(peripheral) = self.peripherals.get(&uuid) {
            Some((peripheral.clone(), self.advertisements.get(&peripheral.id()).unwrap().clone()))
        } else {
            None
        }
    }
}
