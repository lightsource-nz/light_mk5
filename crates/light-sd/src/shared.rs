//! One card, several users: the slot as a SHAREABLE block device, borrowing the board's one
//! [`SpiSd`] per block operation. That is what lets a console command and a long-lived mounted
//! volume (an open recording, say) coexist on one card. [`CardStore`] is the [`Store`] over it --
//! the card brought up on first touch since insertion, forgotten when it fails mid-operation.

use core::cell::RefCell;

use light_core::hal::{BlockDevice, BlockError, Clock, OutputPin, SpiBus, Store};
use light_core::{info, warn};

use crate::{SdError, SpiSd};

fn block_error(e: SdError) -> BlockError {
        match e {
                SdError::Timeout => BlockError::Timeout,
                _ => BlockError::Io,
        }
}

/// A borrow of the shared card, taken per block operation.
pub struct SharedCard<'a, B: SpiBus, O: OutputPin>(pub &'a RefCell<SpiSd<B, O>>);

impl<B: SpiBus, O: OutputPin> Clone for SharedCard<'_, B, O> {
        fn clone(&self) -> Self {
                *self
        }
}
impl<B: SpiBus, O: OutputPin> Copy for SharedCard<'_, B, O> {}

impl<B: SpiBus, O: OutputPin> BlockDevice for SharedCard<'_, B, O> {
        fn block_count(&self) -> u32 {
                self.0.borrow().card.map(|c| c.blocks).unwrap_or(0)
        }
        fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError> {
                self.0.borrow_mut().read_block(lba, out).map_err(block_error)
        }
        fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError> {
                self.0.borrow_mut().write_block(lba, data).map_err(block_error)
        }
}

/// The shared card as a [`Store`]: `open` initialises it on first touch since insertion (an empty
/// slot is ordinary, so that is logged, not warned) and hands a [`SharedCard`]; `reset` forgets it.
pub struct CardStore<B: SpiBus + 'static, O: OutputPin + 'static, C: Clock> {
        pub card: &'static RefCell<SpiSd<B, O>>,
        pub clock: C,
}

impl<B: SpiBus + 'static, O: OutputPin + 'static, C: Clock> Store for CardStore<B, O, C> {
        type Dev = SharedCard<'static, B, O>;

        fn open(&mut self) -> Option<Self::Dev> {
                if self.card.borrow().card.is_none() {
                        if let Err(e) = self.card.borrow_mut().init(&mut self.clock) {
                                info!("card: init failed ({e:?})");
                                return None;
                        }
                }
                Some(SharedCard(self.card))
        }

        fn reset(&mut self) {
                self.card.borrow_mut().card = None;
        }
}

/// Bring the card up from scratch and report it: capacity, kind, and whether block 0 reads and
/// carries a boot signature -- the `sd` console probe.
pub fn report<B: SpiBus, O: OutputPin>(sd: &mut SpiSd<B, O>, clock: &mut dyn Clock) {
        match sd.init(clock) {
                Ok(card) => {
                        let mb = card.blocks / 2048;
                        let kind = if card.high_capacity { "SDHC/XC" } else { "SDSC" };
                        let mut block = [0u8; 512];
                        match sd.read_block(0, &mut block) {
                                Ok(()) => {
                                        let sig = block[510] == 0x55 && block[511] == 0xAA;
                                        info!("sd: {mb} MB {kind} ({} blocks); block 0 read, boot signature {}", card.blocks, if sig { "present" } else { "absent" });
                                }
                                Err(e) => warn!("sd: {mb} MB {kind} identified but block 0 read failed: {e:?}"),
                        }
                }
                Err(e) => info!("sd: {e:?}"),
        }
}
