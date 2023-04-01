//! Peripheral implementations for the SAM4L MCU.
//!
//! <http://www.atmel.com/microsite/sam4l/default.aspx>

#![crate_name = "sam4l"]
#![crate_type = "rlib"]
#![no_std]

pub mod acifc;
pub mod adc;
pub mod aes;
pub mod ast;
pub mod bpm;
pub mod bscif;
pub mod chip;
pub mod crccu;
pub mod dac;
pub mod dma;
pub mod eic;
pub mod flashcalw;
pub mod gloc;
pub mod gpio;
pub mod i2c;
pub mod nvic;
pub mod pm;
pub mod scif;
pub mod serial_num;
pub mod spi;
pub mod trng;
pub mod usart;
pub mod usbc;
pub mod wdt;

pub unsafe fn init() {
    cortexm4::nvic::disable_all();
    cortexm4::nvic::clear_all_pending();
    cortexm4::nvic::enable_all();
}
