use async_std::{task, task::JoinHandle};
use core_bluetooth::central::CentralManager;
use futures::{select, StreamExt};
use postage::{*, prelude::Sink};

use crate::commands::*;
use crate::daemon_state::{DaemonState, handle_event};

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

                    let mut command_receiver = command_receiver.fuse();
                    let mut central_receiver = central_receiver.fuse();

                    let mut state = DaemonState::new();
                    // Events handler loop
                    loop {
                        select! {
                            command = command_receiver.next() => if let Some((command, sender)) = command {
                                command.execute(&mut state, sender, &central)
                            },
                            central_event = central_receiver.next() => if let Some(event) = central_event {
                                handle_event(&mut state, &event, command_sender.clone())
                            }
                        }
                    }
                })
        },
         Controller { sender: command_sender_copy })
    }
}
