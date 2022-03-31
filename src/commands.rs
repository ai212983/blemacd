use std::time::Duration;

use core_bluetooth::central::characteristic::{Characteristic, WriteKind};
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::service::Service;
use core_bluetooth::central::{CentralEvent, CentralManager};
use core_bluetooth::error::Error;
use core_bluetooth::uuid::Uuid;
use log::{error, info};
use postage::mpsc::Sender;
use postage::prelude::Sink;

use crate::daemon_state::{Advertisement, DaemonState};

#[derive(Debug, Clone)]
pub enum CommandResult {
    GetStatus(Duration, Option<usize>),
    ListPeripherals(Vec<(Peripheral, Advertisement)>),
    FindPeripheral(Option<Peripheral>),
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
    FindPeripheralByService(Uuid),
    ConnectToPeripheral(Peripheral),
    RegisterConnectedPeripheral(Peripheral),
    UnregisterConnectedPeripheral(Uuid),
    FindService(Peripheral, String),
    FindCharacteristic(Peripheral, Service, String, uuid::Uuid),
    // last uuid is not BLE uuid, but internal uuid used for hashmap
    ReadCharacteristic(Peripheral, Characteristic),
    WriteCharacteristic(Peripheral, Characteristic, Vec<u8>),
}

// https://stackoverflow.com/questions/49142453/is-it-possible-to-call-a-fnonce-from-a-fn-without-a-mutex
// https://stackoverflow.com/questions/30411594/cannot-move-a-value-of-type-fnonce-when-moving-a-boxed-function

impl Command {
    pub fn execute(
        &self,
        state: &mut DaemonState,
        central: &CentralManager,
        scan_sender: &mut Sender<(Uuid, bool)>,
    ) -> Execution {
        // Sending command:
        // command_sender.try_send((command, sender)).ok();
        //
        // Sending result:
        // sender.blocking_send(result).unwrap();

        match self {
            Command::GetStatus => Execution::Result(CommandResult::GetStatus(
                Duration::new(state.started_at.elapsed().as_secs(), 0),
                Some(state.peripherals.len()),
            )),

            Command::ListPeripherals => Execution::Result(CommandResult::ListPeripherals({
                state
                    .peripherals
                    .values()
                    .into_iter()
                    .map(|p| {
                        return (
                            p.clone(),
                            state.advertisements.get(&p.id()).unwrap().clone(),
                        );
                    })
                    .collect()
            })),

            Command::FindPeripheralByService(uuid) => {
                info!("looking for peripheral with service [{}]", uuid);
                if let Some(result) = state.find_connected_peripheral_by_service(uuid.clone()) {
                    info!("found already connected peripheral [{}]", result.id());
                    Execution::Result(CommandResult::FindPeripheral(Some(result)))
                } else {
                    info!("no matching peripheral found, starting scan");
                    let uuid = uuid.clone();

                    scan_sender.blocking_send((uuid, true)).unwrap();

                    // TODO(df): Add timeout and Ctrl+C handling

                    return Execution::Matcher(Box::new(move |event| {
                        if let CentralEvent::PeripheralDiscovered {
                            peripheral,
                            advertisement_data,
                            rssi: _,
                        } = event
                        {
                            if advertisement_data
                                .service_uuids()
                                .iter()
                                .find(|&u| u.eq(&uuid))
                                .is_some()
                            {
                                return EventMatchResult::Result(CommandResult::FindPeripheral(
                                    Some(peripheral.clone()),
                                ));
                            }
                        }
                        EventMatchResult::NoMatch
                    }));
                }
            }

            Command::ConnectToPeripheral(ref peripheral) => {
                let id = peripheral.id().clone();
                if let Some(result) = state.get_peripheral(id) {
                    Execution::Result(CommandResult::ConnectToPeripheral(result.0))
                } else {
                    let peripheral_uuid = peripheral.id();
                    central.connect(&peripheral);
                    Execution::Matcher(Box::new(move |event| {
                        if let CentralEvent::PeripheralConnected { peripheral } = event {
                            if peripheral.id() == peripheral_uuid {
                                return EventMatchResult::Command(
                                    Command::RegisterConnectedPeripheral(peripheral.clone()),
                                );
                            }
                        } else if let CentralEvent::PeripheralConnectFailed { peripheral, error } =
                            event
                        {
                            if peripheral.id() == peripheral_uuid {
                                let id = peripheral.id().clone();
                                error!(
                                    "failed to connect to peripheral {:?}: {:?}",
                                    peripheral, error
                                );
                                return EventMatchResult::Command(
                                    Command::UnregisterConnectedPeripheral(id),
                                );
                            }
                        }
                        EventMatchResult::NoMatch
                    }))
                }
            }

            Command::RegisterConnectedPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                state.peripherals.insert(id, peripheral.clone());
                info!(
                    "peripheral [{}]{} registered, total {}",
                    id,
                    state
                        .advertisements
                        .get(&id)
                        .and_then(|ad| ad.data.local_name())
                        .map_or(String::new(), |s| format!(" ({})", s)),
                    state.peripherals.len()
                );

                Execution::Result(CommandResult::ConnectToPeripheral(peripheral.clone()))
            }

            Command::UnregisterConnectedPeripheral(uuid) => {
                state.peripherals.remove(&uuid);
                state.advertisements.remove(&uuid);
                Execution::None
            }

            Command::FindService(ref peripheral, uuid_substr) => {
                peripheral.discover_services();

                let id = peripheral.id();
                let uuid_substr = uuid_substr.clone();

                Execution::Matcher(Box::new(move |event| {
                    if let CentralEvent::ServicesDiscovered {
                        peripheral,
                        services,
                    } = event
                    {
                        if peripheral.id() == id {
                            let find_fn = get_finder_by_id_substr(&uuid_substr);
                            return EventMatchResult::Result(CommandResult::FindService(
                                peripheral.clone(),
                                find_fn(services),
                            ));
                        }
                    }
                    EventMatchResult::NoMatch
                }))
            }

            Command::FindCharacteristic(ref peripheral, ref service, uuid_substr, _) => {
                peripheral.discover_characteristics(service);

                let peripheral_id = peripheral.id();
                let service_id = service.id();
                let uuid_substr = uuid_substr.clone();

                Execution::Matcher(Box::new(move |event| {
                    if let CentralEvent::CharacteristicsDiscovered {
                        peripheral,
                        service,
                        characteristics,
                    } = event
                    {
                        if peripheral.id() == peripheral_id && service.id() == service_id {
                            let find_fn = get_finder_by_id_substr(&uuid_substr);
                            return EventMatchResult::Result(CommandResult::FindCharacteristic(
                                peripheral.clone(),
                                find_fn(characteristics),
                            ));
                        }
                    }
                    EventMatchResult::NoMatch
                }))
            }
            Command::ReadCharacteristic(ref peripheral, ref characteristic) => {
                peripheral.read_characteristic(characteristic);

                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Execution::Matcher(Box::new(move |event| {
                    if let CentralEvent::CharacteristicValue {
                        peripheral,
                        characteristic,
                        value,
                    } = event
                    {
                        if peripheral.id() == peripheral_id
                            && characteristic.id() == characteristic_id
                        {
                            return EventMatchResult::Result(CommandResult::ReadCharacteristic(
                                peripheral.clone(),
                                characteristic.clone(),
                                if let Ok(value) = value {
                                    Some(value.clone())
                                } else {
                                    None
                                },
                            ));
                        }
                    }
                    EventMatchResult::NoMatch
                }))
            }
            Command::WriteCharacteristic(ref peripheral, ref characteristic, ref data) => {
                peripheral.write_characteristic(characteristic, data, WriteKind::WithResponse);

                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Execution::Matcher(Box::new(move |event| {
                    if let CentralEvent::WriteCharacteristicResult {
                        peripheral,
                        characteristic,
                        result,
                    } = event
                    {
                        if peripheral.id() == peripheral_id
                            && characteristic.id() == characteristic_id
                        {
                            return EventMatchResult::Result(CommandResult::WriteCharacteristic(
                                peripheral.clone(),
                                characteristic.clone(),
                                result.clone(),
                            ));
                        }
                    }
                    EventMatchResult::NoMatch
                }))
            }
        }
    }
}

pub enum Execution {
    Result(CommandResult),
    Matcher(EventMatcher),
    None,
}

pub enum EventMatchResult {
    Command(Command),
    Result(CommandResult),
    NoMatch,
}

pub type EventMatcher = Box<dyn Fn(&CentralEvent) -> EventMatchResult + Send>;

trait HasId {
    fn get_id(&self) -> Uuid;
}

impl HasId for Service {
    fn get_id(&self) -> Uuid {
        self.id().clone()
    }
}

impl HasId for Characteristic {
    fn get_id(&self) -> Uuid {
        self.id().clone()
    }
}

fn get_finder_by_id_substr<T>(id_substr: &String) -> impl Fn(&Result<Vec<T>, Error>) -> Option<T>
where
    T: HasId + Clone,
{
    let id_substr = id_substr.clone();
    let result = move |items: &Result<Vec<T>, Error>| -> Option<T> {
        let id_substr = id_substr.as_str();
        return items
            .as_ref()
            .ok()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get_id().to_string().contains(id_substr))
            })
            .map(|item| item.clone());
    };
    return result;
}
