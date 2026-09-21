// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `hal_flash_driver::FlashDriver` bridge for [`SpiNorFlash`].
//!
//! This is what lets AST10x0 SPI NOR flash be reached through the portable
//! `hal_flash::Flash` interface (via `BlockingFlash`), so services that only
//! know the HAL — storage, firmware update — can talk to real hardware.
//!
//! The SMC device layer polls the flash status register inside
//! `erase_sector`/`program_page`, so erase and program are already complete by
//! the time `start_erase`/`start_program` return. `is_busy` is therefore always
//! false and `complete_op` is a no-op; pair the driver with
//! [`ImmediateBlocking`] rather than an interrupt-backed `Blocking`.
//!
//! ```ignore
//! let mut nor = SpiNorFlash::from_fmc_cs(&mut fmc, cfg, ChipSelect::Cs1)?;
//! let mut flash = BlockingFlash {
//!     driver: SpiNorFlashDriver::new(&mut nor)?,
//!     blocking: ImmediateBlocking,
//! };
//! flash.program(FlashAddress::new(0x10_0003), b"unaligned is fine")?;
//! ```

use core::num::NonZero;

use hal_flash_driver::{FlashAddress, FlashDriver};
use util_types::{Blocking, PowerOf2Usize};

use crate::smc::device::flash::{SpiNorFlash, SpiNorFlashDevice};
use crate::smc::types::SmcError;

/// Erase granularity exposed to the HAL. The device layer only implements the
/// 4 KiB sector erase opcode, so that is the single erasable size advertised.
const SECTOR_SIZE: usize = 4096;

/// SPI NOR page-program window. A program must not cross this boundary.
const PAGE_PROGRAM_SIZE: usize = 256;

/// A [`Blocking`] that never waits, for drivers whose operations are already
/// synchronous by the time they return.
pub struct ImmediateBlocking;

impl Blocking for ImmediateBlocking {
    fn wait_for_notification(&self) {}
}

/// `FlashDriver` view of a [`SpiNorFlash`].
///
/// Capacity is captured at construction because `FlashDriver::size` cannot
/// fail, while querying the controller can.
pub struct SpiNorFlashDriver<'a, 'b> {
    flash: &'a mut SpiNorFlash<'b>,
    size: NonZero<usize>,
}

impl<'a, 'b> SpiNorFlashDriver<'a, 'b> {
    /// Wrap a flash device for use through the HAL.
    ///
    /// The `FlashDriver` geometry constants are compile-time, so the device's
    /// runtime geometry must match them: 256-byte pages and 4 KiB sectors.
    /// Anything else is rejected rather than silently mis-programmed.
    pub fn new(flash: &'a mut SpiNorFlash<'b>) -> Result<Self, SmcError> {
        let cfg = flash.config();
        if cfg.page_size as usize != PAGE_PROGRAM_SIZE || cfg.sector_size as usize != SECTOR_SIZE {
            return Err(SmcError::DeviceNotSupported);
        }
        let size = NonZero::new(SpiNorFlashDevice::capacity_bytes(flash)?)
            .ok_or(SmcError::InvalidCapacity)?;
        Ok(Self { flash, size })
    }
}

impl FlashDriver for SpiNorFlashDriver<'_, '_> {
    type Error = SmcError;

    /// Smallest erase block, matching the single bit set in the erasable bitmap.
    const PAGE_SIZE: usize = SECTOR_SIZE;
    const PROGRAM_WINDOW_SIZE: usize = PAGE_PROGRAM_SIZE;
    /// Reads are a memcpy out of the memory-mapped flash window, so the
    /// hardware imposes no transfer limit or alignment.
    const MAX_READ_SIZE: usize = usize::MAX;
    const READ_ALIGNMENT: usize = 1;
    const PROGRAM_ALIGNMENT: usize = 1;

    fn size(&self) -> NonZero<usize> {
        self.size
    }

    fn erasable_sizes_bitmap(&mut self) -> Result<u32, Self::Error> {
        Ok(1 << SECTOR_SIZE.trailing_zeros())
    }

    fn read(&mut self, start_addr: FlashAddress, buf: &mut [u8]) -> Result<(), Self::Error> {
        let read = SpiNorFlashDevice::read(self.flash, start_addr.offset(), buf)?;
        if read != buf.len() {
            // A short read would leave stale bytes in `buf` and silently
            // corrupt anything verifying against it.
            return Err(SmcError::HardwareError);
        }
        Ok(())
    }

    fn start_erase(
        &mut self,
        start_addr: FlashAddress,
        size: PowerOf2Usize,
    ) -> Result<(), Self::Error> {
        if size.get() != SECTOR_SIZE {
            return Err(SmcError::InvalidCapacity);
        }
        self.flash.erase_sector(start_addr.offset())
    }

    fn start_program(
        &mut self,
        start_address: FlashAddress,
        data: &[u8],
    ) -> Result<(), Self::Error> {
        self.flash
            .program_page(start_address.offset(), data)
            .map(|_| ())
    }

    fn is_busy(&mut self) -> bool {
        false
    }

    fn complete_op(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
