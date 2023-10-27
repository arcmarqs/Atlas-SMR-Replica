use std::marker::PhantomData;
use atlas_common::channel;
use atlas_common::channel::{ChannelSyncRx, ChannelSyncTx, OneShotTx};
use atlas_common::error::*;
use atlas_common::ordering::SeqNo;
use atlas_communication::message::StoredMessage;
use atlas_core::ordering_protocol::ExecutionResult;
use atlas_core::ordering_protocol::networking::serialize::NetworkView;
use atlas_core::state_transfer::{StateTransferProtocol, STMsg, STPollResult, STResult};
use atlas_core::state_transfer::networking::serialize::StateTransferMessage;
use atlas_core::timeouts::RqTimeout;

pub const WORK_CHANNEL_SIZE: usize = 128;
pub const RESPONSE_CHANNEL_SIZE: usize = 128;

/// A state transfer work message
pub enum StateTransferWorkMessage<V, ST> where V: NetworkView {
    RequestLatestState(V),
    StateTransferMessage(V, StoredMessage<ST>),
    Timeout(V, Vec<RqTimeout>),
    ShouldRequestAppState(V, SeqNo, OneShotTx<ExecutionResult>),
}

/// Response message of the state transfer protocol
pub enum StateTransferProgress {
    StateTransferProgress(STResult),
    CheckpointReceived(SeqNo),
}

/// The handle to the state transfer thread.
#[derive(Clone)]
pub struct StateTransferThreadHandle<V, ST> where V: NetworkView, ST: StateTransferMessage {
    work_tx: ChannelSyncTx<StateTransferWorkMessage<V, STMsg<ST>>>,
    response_rx: ChannelSyncRx<StateTransferProgress>,
}

#[derive(Clone)]
pub struct StateTransferThreadInnerHandle<V, ST> where V: NetworkView, ST: StateTransferMessage {
    work_rx: ChannelSyncRx<StateTransferWorkMessage<V, STMsg<ST>>>,
    response_tx: ChannelSyncTx<StateTransferProgress>,
}

/// The state transfer management struct, contains the base work handles to deliver work to the
/// state transfer module
pub struct StateTransferMngr<V, S, NT, PL, ST>
    where ST: StateTransferProtocol<S, NT, PL>,
          V: NetworkView {
    handle: StateTransferThreadInnerHandle<V, ST::Serialization>,
    currently_running: bool,
    phantom: PhantomData<(S, NT, PL)>,
}

impl<V, S, NT, PL, ST> StateTransferMngr<V, S, NT, PL, ST>
    where ST: StateTransferProtocol<S, NT, PL>,
          V: NetworkView {
    pub fn init_state_transfer_handles() -> (StateTransferThreadHandle<V, ST::Serialization>, StateTransferThreadInnerHandle<V, ST::Serialization>) {
        let (work_tx, work_rx) = channel::new_bounded_sync(WORK_CHANNEL_SIZE, Some("State transfer Work Channel"));
        let (response_tx, response_rx) = channel::new_bounded_sync(RESPONSE_CHANNEL_SIZE, Some("State Transfer Response Channel"));

        (StateTransferThreadHandle {
            work_tx,
            response_rx,
        }, StateTransferThreadInnerHandle {
            work_rx,
            response_tx,
        })
    }

    pub(crate) fn initialize_core_state_transfer(state_handle: StateTransferThreadInnerHandle<V, ST::Serialization>) -> Result<Self> {
        Ok(Self {
            handle: state_handle,
            currently_running: false,
            phantom: Default::default(),
        })
    }

    pub fn notify_of_checkpoint(&self, seq: SeqNo) {
        self.handle.response_tx.send(StateTransferProgress::CheckpointReceived(seq)).unwrap()
    }

    pub fn iterate(&mut self, state_transfer: &mut ST) {
        while let Ok(work) = self.handle.work_rx.try_recv() {
            match work {
                StateTransferWorkMessage::ShouldRequestAppState(view, seq, response) =>
                    Self::should_request_app_state(view, seq, state_transfer, response),
                StateTransferWorkMessage::RequestLatestState(view) => {
                    self.currently_running = true;

                    Self::request_latest_state(view, state_transfer);
                }
                StateTransferWorkMessage::StateTransferMessage(view, message) => {
                    if self.currently_running {
                        let result = state_transfer.process_message(view, message);

                        if let Ok(st_result) = result {
                            let _ = self.handle.response_tx.send(StateTransferProgress::StateTransferProgress(st_result));
                        }
                    } else {
                        let _ = state_transfer.handle_off_ctx_message(view, message);
                    }
                }
                StateTransferWorkMessage::Timeout(view, timeout) => {}
            }
        }

        if self.currently_running {
            match state_transfer.poll()? {
                STPollResult::ReceiveMsg => {}
                STPollResult::RePoll => {
                    return;
                }
                STPollResult::Exec(message) => {
                    let res = state_transfer.process_message(self.view(), message)?;

                    let _ = self.handle.response_tx.send(StateTransferProgress::StateTransferProgress(res));
                }
                STPollResult::STResult(res) => {
                    let _ = self.handle.response_tx.send(StateTransferProgress::StateTransferProgress(res));
                }
            };
        }
    }

    fn request_latest_state(view: V, state_transfer: &mut ST) {
        let _ = state_transfer.request_latest_state(view);
    }

    fn should_request_app_state(view: V, seq: SeqNo, state_transfer: &mut ST, result: OneShotTx<ExecutionResult>) {
        let appstate_res = state_transfer.handle_app_state_requested(view, seq);

        if let Ok(appstate) = appstate_res {
            let _ = result.send(appstate);
        } else {
            let _ = result.send(ExecutionResult::Nil);
        }
    }
}

impl<V, ST> StateTransferThreadHandle<V, ST> where V: NetworkView,
                                                   ST: StateTransferMessage {
    pub fn send_work_message(&self, msg: StateTransferWorkMessage<V, STMsg<ST>>) {
        let _ = self.work_tx.send(msg);
    }

    pub fn receive_state_transfer_update(&self) -> StateTransferProgress {
        self.response_rx.recv().unwrap()
    }

    pub fn try_recv_state_transfer_update(&self) -> Option<StateTransferProgress> {
        match self.response_rx.try_recv() {
            Ok(progress) => {
                Some(progress)
            }
            Err(_) => {
                None
            }
        }
    }
}