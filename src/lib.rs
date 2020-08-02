#![feature(test)]
use smol::prelude::*;
mod crypt;
mod fec;
mod msg;
mod session;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
