//! Finding every function behind a window.

use crate::address::{Address, DEVICES, FUNCTIONS};
use crate::config::Config;
use crate::function::Function;

/// Every function that answers behind `config`.
///
/// The sweep covers exactly the bus range the window maps, rather than
/// following bridges from bus zero. A bridge walk asks the bridges themselves
/// which buses live behind them, and believes the answer; the sweep asks the
/// same question of every address the platform already mapped and audited, and
/// finds the same functions without the extra trust. It also cannot loop, which
/// a misprogrammed secondary-bus number can make a walk do.
///
/// Nothing is allocated: the scan is an iterator over addresses, and each item
/// borrows the window it was found through.
pub fn scan<C: Config + ?Sized>(config: &C) -> Scan<'_, C> {
    let (first, last) = config.buses();
    Scan {
        config,
        bus: u16::from(first),
        last: u16::from(last),
        device: 0,
        function: 0,
        multifunction: false,
    }
}

/// The functions found by [`scan`], in address order.
pub struct Scan<'c, C: Config + ?Sized> {
    config: &'c C,
    bus: u16,
    last: u16,
    device: u8,
    function: u8,
    multifunction: bool,
}

impl<'c, C: Config + ?Sized> Iterator for Scan<'c, C> {
    type Item = Function<'c, C>;

    fn next(&mut self) -> Option<Function<'c, C>> {
        while self.bus <= self.last {
            let at = Address::new(self.bus as u8, self.device, self.function)
                .expect("the sweep never leaves the encodable range");
            let found = Function::probe(self.config, at).ok();
            if self.function == 0 {
                // Only function zero says whether the others are worth probing,
                // and a device that does not answer there answers nowhere.
                self.multifunction = found.is_some_and(Function::multifunction);
            }
            self.step();
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

impl<C: Config + ?Sized> Scan<'_, C> {
    fn step(&mut self) {
        self.function += 1;
        if !self.multifunction || self.function == FUNCTIONS {
            self.function = 0;
            self.device += 1;
        }
        if self.device == DEVICES {
            self.device = 0;
            self.bus += 1;
        }
    }
}
