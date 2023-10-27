use std::sync::Arc;
use std::time::Instant;
use atlas_common::channel::{ChannelSyncRx, ChannelSyncTx};
use atlas_common::error::*;
use atlas_core::ordering_protocol::networking::serialize::NetworkView;
use atlas_core::persistent_log::DivisibleStateLog;
use atlas_core::state_transfer::divisible_state::DivisibleStateTransfer;
use atlas_core::state_transfer::StateTransferProtocol;
use atlas_core::timeouts::Timeouts;
use atlas_metrics::metrics::metric_duration;
use atlas_persistent_log::PersistentLog;
use atlas_smr_application::state::divisible_state::{AppState, AppStateMessage, DivisibleState, InstallStateMessage};
use crate::metric::STATE_TRANSFER_PROCESS_TIME_ID;
use crate::server::state_transfer::{StateTransferMngr, StateTransferThreadHandle, StateTransferThreadInnerHandle};

pub struct DivStateTransfer<V, S, NT, PL, ST>
    where V: NetworkView,
          S: DivisibleState + 'static,
          ST: DivisibleStateTransfer<S, NT, PL>,
          PL: DivisibleStateLog<S> {
    inner_state: StateTransferMngr<V, S, NT, PL, ST>,

    state_tx_to_executor: ChannelSyncTx<InstallStateMessage<S>>,
    checkpoint_rx_from_app: ChannelSyncRx<AppStateMessage<S>>,

    state_transfer_protocol: ST,
}

impl<V, S, NT, PL, ST> DivStateTransfer<V, S, NT, PL, ST>
    where V: NetworkView,
          S: DivisibleState + 'static,
          ST: DivisibleStateTransfer<S, NT, PL>,
          PL: DivisibleStateLog<S> {
    pub fn init_state_transfer_thread(state_tx: ChannelSyncTx<InstallStateMessage<S>>,
                                      checkpoint_rx: ChannelSyncRx<AppStateMessage<S>>,
                                      st_config: ST::Config,
                                      node: Arc<NT>,
                                      timeouts: Timeouts,
                                      persistent_log: PL,
                                      handle: StateTransferThreadInnerHandle<V, ST::Serialization>) {
        let inner_mngr = StateTransferMngr::initialize_core_state_transfer(handle)?;

        let state_transfer_protocol = ST::initialize(st_config, timeouts,
                                                     node, persistent_log,
                                                     state_tx.clone())?;

        let state_transfer_manager = Self {
            inner_state: inner_mngr,
            state_tx_to_executor: state_tx,
            checkpoint_rx_from_app: checkpoint_rx,
            state_transfer_protocol,
        };

    }

    pub fn run(mut self) -> Result<()> {
        let mut last_loop = Instant::now();

        loop {
            self.receive_checkpoints()?;

            self.inner_state.iterate(&mut self.state_transfer_protocol);

            metric_duration(STATE_TRANSFER_PROCESS_TIME_ID, last_loop.elapsed());

            last_loop = Instant::now();
        }
    }

    /// Receive checkpoints from the application layer
    fn receive_checkpoints(&mut self) -> Result<()> {
        while let Ok(checkpoint) = self.checkpoint_rx_from_app.try_recv() {
            let (seq_no, state) = checkpoint.into_state();

            match state {
                AppState::StateDescriptor(descriptor) => {
                    self.state_transfer_protocol.handle_state_desc_received_from_app(descriptor)?;
                }
                AppState::StatePart(parts) => {
                    self.state_transfer_protocol.handle_state_part_received_from_app(parts.into_vec())?;
                }
                AppState::Done => {
                    self.state_transfer_protocol.handle_state_finished_reception()?;
                    self.inner_state.notify_of_checkpoint(seq_no)?;
                }
            }
        }

        Ok(())
    }
}