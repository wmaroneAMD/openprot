// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use ast10x0_board::{Ast10x0Board, Ast10x0BoardDescriptor, I2cBusCfg};
use ast10x0_peripherals::i2c::{ClockConfig, I2cConfig, I2cSpeed, I2cXferMode};
use ast10x0_peripherals::scu::pinctrl;
use console_backend::console_backend_write_all;
use entry as _;
use target_common::{declare_target, TargetInterface};

const I2C_BUS_CFG: I2cConfig = I2cConfig {
    speed: I2cSpeed::Standard,
    xfer_mode: I2cXferMode::DmaMode,
    multi_master: false,
    smbus_timeout: false,
    smbus_alert: false,
    clock_config: ClockConfig::ast1060_default(),
};

// This source is compiled into two kernel binaries — `:target` (requester,
// codegen from system.json5) and `:target_peer` (responder, codegen from
// peer_system.json5) — so it brings up every bus either image may open:
//   bus 2 (SCL3/SDA3, GPIOI0/I1)  — requester, harness J15 daughter-to-daughter
//   bus 8 (SCL9/SDA9, GPIOJ4/J5)  — responder, routes to the parent AST2600's
//                                   SCL1/SDA1 (B20/A20) = BMC /dev/i2c-0
// Each app opens only its own bus; the unused controller sits idle. The IRQ
// binding is what actually differs, and that comes from each kernel's codegen.
static PINCTRL_GROUPS: [&[ast10x0_peripherals::scu::PinctrlPin]; 2] =
    [pinctrl::PINCTRL_I2C2, pinctrl::PINCTRL_I2C8];
static I2C_BUSES: [I2cBusCfg; 2] = [
    I2cBusCfg {
        bus: 2,
        config: I2C_BUS_CFG,
    },
    I2cBusCfg {
        bus: 8,
        config: I2C_BUS_CFG,
    },
];

pub struct Target;

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 SPDM Auth Stress Test";

    fn main() -> ! {
        // SAFETY: kernel main() runs once with exclusive hardware ownership.
        if unsafe {
            Ast10x0Board::new(Ast10x0BoardDescriptor {
                pinctrl_groups: &PINCTRL_GROUPS,
                i2c_buses: &I2C_BUSES,
            })
            .init()
        }
        .is_err()
        {
            loop {}
        }

        codegen::start();
        loop {}
    }

    fn shutdown(code: u32) -> ! {
        let sentinel: &[u8] = if code == 0 {
            b"TEST_RESULT:PASS\n"
        } else {
            b"TEST_RESULT:FAIL\n"
        };
        let _ = console_backend_write_all(sentinel);
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
