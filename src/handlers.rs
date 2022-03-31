use std::collections::{HashMap, HashSet};

use async_std::{task, task::JoinHandle};
use core_bluetooth::central::{CentralManager, ScanOptions};
use core_bluetooth::uuid::Uuid as BtUuid;
use futures::{select, StreamExt};
use log::*;
use postage::{prelude::Sink, *};
use uuid::Uuid;

use crate::commands::*;
use crate::daemon_state::DaemonState;

const CHANNEL_CAPACITY: usize = 100;

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
            sender: self.sender.clone(),
        }
    }
}

pub struct AsyncManager {
    io_handle: JoinHandle<()>,
    processing_handle: JoinHandle<()>,
}

impl AsyncManager {
    pub fn new() -> (Self, Controller) {
        let (input_command_sender, input_command_receiver) =
            mpsc::channel::<(Command, oneshot::Sender<CommandResult>)>(CHANNEL_CAPACITY);
        let (mut command_sender, command_receiver) =
            mpsc::channel::<(Command, Uuid)>(CHANNEL_CAPACITY);
        let (mut output_sender, output_receiver) =
            mpsc::channel::<(CommandResult, Uuid)>(CHANNEL_CAPACITY);

        let mut command_sender_internal = command_sender.clone();
        (
            Self {
                io_handle: task::spawn(async move {
                    let mut senders = HashMap::<Uuid, oneshot::Sender<CommandResult>>::new();
                    let mut input_receiver = input_command_receiver.fuse();
                    let mut output_receiver = output_receiver.fuse();

                    loop {
                        select! {
                           command = input_receiver.next() => if let Some((command, sender)) = command {
                                let uuid = Uuid::new_v4();
                                senders.insert(uuid, sender);
                                command_sender.send((command, uuid)).await.ok();
                            },
                            result = output_receiver.next() => if let Some((result, uuid)) = result {
                                if let Some(mut sender) = senders.remove(&uuid) {
                                    sender.send(result).await.ok();
                                }
                            }
                        }
                    }
                }),
                processing_handle: task::spawn(async move {
                    let (mut scan_sender, scan_receiver) =
                        mpsc::channel::<(BtUuid, bool)>(CHANNEL_CAPACITY);
                    let (central, central_receiver) = CentralManager::new();

                    let mut command_receiver = command_receiver.fuse();
                    let mut scan_receiver = scan_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();

                    let mut matchers = HashMap::<Uuid, EventMatcher>::new();
                    let mut uuids_to_scan = HashSet::<BtUuid>::new();
                    let mut state = DaemonState::new();

                    loop {
                        select! {
                            scan = scan_receiver.next() => if let Some((uuid, start_scan)) = scan {
                                if if start_scan {
                                    uuids_to_scan.insert(uuid)
                                } else {
                                    uuids_to_scan.remove(&uuid)
                                } {
                                    if uuids_to_scan.is_empty() {
                                        central.cancel_scan();
                                    } else {
                                        let uuids: Vec<BtUuid> = uuids_to_scan.iter().cloned().collect();
                                        central.scan_with_options(
                                            ScanOptions::default()
                                            .include_services(&uuids));
                                    }
                                }
                            },

                             command = command_receiver.next() => if let Some((command, uuid)) = command {
                                 match command.execute(&mut state, &central, &mut scan_sender) {
                                     Execution::Result(result) => {
                                         output_sender.send((result, uuid)).await.ok();
                                     },
                                     Execution::Matcher(matcher) => {
                                         matchers.insert(uuid, matcher);
                                     },
                                     Execution::None => {
                                             // do nothing
                                     }
                                }
                             },

                             central_event = central_receiver.next() => if let Some(event) = central_event {
                                 state.handle_event(&event);
                                 matchers.retain(|uuid, matcher| {
                                     match matcher(&event) {
                                         EventMatchResult::Result(result) => {
                                             output_sender.blocking_send((result, *uuid)).unwrap();
                                             false
                                         },
                                         EventMatchResult::Command(command) => {
                                             command_sender_internal.blocking_send((command, *uuid)).unwrap();
                                             false
                                         },
                                         EventMatchResult::NoMatch => {
                                             true
                                         }
                                     }
                                 });
                            }
                        }
                    }
                }),
            },
            Controller {
                sender: input_command_sender,
            },
        )
    }
}
