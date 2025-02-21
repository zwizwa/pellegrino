//! A USB-Serial driver for the nRF52840

use core::ops::Deref;

use bbqueue::{BBBuffer, Consumer, Producer};
use nrf52840_hal::{usbd::{Usbd, UsbPeripheral}, pac::USBD};
use sportty::{Message, max_encoding_length};
use usb_device::{device::UsbDevice, UsbError};
use usbd_serial::SerialPort;
use heapless::{LinearMap, Deque};
use crate::alloc::{HeapArray, HEAP};

const USB_BUF_SZ: usize = 4096;
static UART_INC: BBBuffer<USB_BUF_SZ> = BBBuffer::new();
static UART_OUT: BBBuffer<USB_BUF_SZ> = BBBuffer::new();

/// A type alias for the nRF52840 USB Peripheral type
pub type AUsbPeripheral = Usbd<UsbPeripheral<'static>>;

/// A type alias for the nRF52840 USB Device type
pub type AUsbDevice = UsbDevice<'static, AUsbPeripheral>;

/// A type alias for the nRF52840 CDC-ACM USB Serial port type
pub type ASerialPort = SerialPort<'static, AUsbPeripheral>;

/// The handle necessary for servicing USB interrupts
pub struct UsbUartIsr {
    dev: AUsbDevice,
    ser: ASerialPort,
    out: Consumer<'static, USB_BUF_SZ>,
    inc: Producer<'static, USB_BUF_SZ>,
}

impl UsbUartIsr {
    /// Service the USB ISR, which is triggered by either a regular polling timer,
    /// or some kind of USB interrupt.
    pub fn poll(&mut self) {
        // Service the relevant hardware logic
        self.dev.poll(&mut [&mut self.ser]);

        // If there is data to be sent...
        if let Ok(rgr) = self.out.read() {
            match self.ser.write(&rgr) {
                // ... and there is room to send it, then send it.
                Ok(sz) if sz > 0 => {
                    rgr.release(sz);
                },
                // ... and there is no room to send it, then just bail.
                Ok(_) | Err(UsbError::WouldBlock) => {
                    // Just silently drop the read grant
                }
                // ... and there is a USB error, then panic.
                Err(_) => defmt::panic!("Usb Error Write!"),
            }
        }

        // If there is room to receive data...
        if let Ok(mut wgr) = self.inc.grant_max_remaining(128) {
            match self.ser.read(&mut wgr) {
                // ... and there is data to be read, then take it.
                Ok(sz) if sz > 0 => {
                    wgr.commit(sz);
                },
                // ... and there is no data to be read, then just bail.
                Ok(_) | Err(UsbError::WouldBlock) => {
                    // Just silently drop the write grant
                }
                // ... and there is a USB error, then panic.
                Err(_) => defmt::panic!("Usb Error Read!"),
            }
        }
    }
}

/// The "userspace" handle for the driver
pub struct UsbUartSys {
    out: Producer<'static, USB_BUF_SZ>,
    inc: Consumer<'static, USB_BUF_SZ>,
    // TODO: There's probably a smarter way to handle this without having
    // a bigass accumulator struct in here. Either limit max size, or use
    // a smarter stream decoder which can emit partial data on the fly
    acc: Accumulator<1024>,

    // Also, we might want to "coverge" older messages into fewer allocs,
    // to avoid small chunks filling up the queue
    ports: LinearMap<u16, Deque<HeapArray<u8>, 16>, 8>,
}

/// A struct containing both the "interrupt" and "userspace" handles
/// for this USB-Serial driver
pub struct UsbUartParts {
    pub isr: UsbUartIsr,
    pub sys: UsbUartSys,
}

/// Obtain the "userspace" and "interrupt" portions of the USB-Serial driver
///
/// This only returns `Ok` once, as this driver is a singleton. Subsequent
/// calls will return an `Err`.
pub fn setup_usb_uart(dev: AUsbDevice, ser: ASerialPort) -> Result<UsbUartParts, ()> {
    let (inc_prod, inc_cons) = UART_INC.try_split().map_err(drop)?;
    let (out_prod, out_cons) = UART_OUT.try_split().map_err(drop)?;

    // Port zero (stdio) is always mapped.
    let mut ports = LinearMap::new();
    ports.insert(0, Deque::new()).ok();

    Ok(UsbUartParts {
        isr: UsbUartIsr {
            dev,
            ser,
            out: out_cons,
            inc: inc_prod,
        },
        sys: UsbUartSys {
            out: out_prod,
            inc: inc_cons,
            acc: Accumulator::new(),
            ports,
        }
    })
}

// Implement the "userspace" traits for the USB UART
impl crate::traits::Serial for UsbUartSys {
    fn register_port(&mut self, port: u16) -> Result<(), ()> {
        if self.ports.contains_key(&port) {
            return Err(());
        }

        self.ports.insert(port, Deque::new()).map_err(drop)?;

        defmt::println!("Registered port {=u16}!", port);

        Ok(())
    }

    fn release_port(&mut self, port: u16) -> Result<(), ()> {
        if port == 0 {
            return Err(());
        }

        if self.ports.remove(&port).is_some() {
            Ok(())
        } else {
            Err(())
        }
    }

    fn process(&mut self) {
        // Process all incoming message and dispatch to queues
        'outer: while let Ok(rgr) = self.inc.read() {
            let mut window = rgr.deref();
            let rec_len = rgr.len();

            //////////////////////
            // No early returns here! We need to release the grant!
            while !window.is_empty() {
                match self.acc.feed(window) {
                    Ok(Some(mut msg)) => {
                        match Message::decode_in_place(msg.msg.as_mut_slice()) {
                            Ok(smsg) => {
                                // defmt::println!("Decoded port {=u16} - msg: {=[u8]}", smsg.port, smsg.data);

                                // If this is port 0, then (try to) also loopback!
                                // #[cfg(feature = "auto-loopback")]
                                if smsg.port == 0 {
                                    self.send(0, &smsg.data).ok();
                                }

                                // TODO: Replace this with `map()` and Results so we can actually
                                // tell which part went wrong
                                let failed = self.ports
                                    .get_mut(&smsg.port)
                                    .and_then(|dq| {
                                        // Keep the heap locked for as short as possible!
                                        let mut hp = HEAP.try_lock()?;
                                        let habox = hp.alloc_box_array(0u8, smsg.data.len()).ok()?;
                                        Some((dq, habox))
                                    })
                                    .and_then(|(dq, mut habox)| {
                                        habox.copy_from_slice(&smsg.data);
                                        dq.push_back(habox).ok()
                                    }).is_none();

                                if failed && self.ports.contains_key(&smsg.port) {
                                    defmt::println!("Failed to receive message for serial port {=u16}. Discarding.", smsg.port);
                                }
                            },
                            Err(_) => defmt::println!("Sportty error!"),
                        }
                        window = msg.remainder;
                    },
                    Ok(None) => {},
                    Err(AccError::NoRoomNoRem) => {
                        rgr.release(rec_len);
                        continue 'outer;
                    },
                    Err(AccError::NoRoomWithRem(rem)) => {
                        window = rem;
                    }
                }
            }

            rgr.release(rec_len);
            // End of "no early return" zone!
            //////////////////////
        }
    }

    fn recv<'a>(&mut self, port: u16, buf: &'a mut [u8]) -> Result<&'a mut [u8], ()> {
        self.process();

        let deq = self.ports.get_mut(&port).ok_or(())?;
        let mut used = 0;
        let buflen = buf.len();

        while used < buf.len() {
            let msg = match deq.pop_front() {
                None => {
                    // No more queued contents, bail!
                    //
                    // NOTE: `&mut buf[..0]` does correctly give back `&mut []`
                    // (and not a slice panic) as you may expect - I checked :)
                    return Ok(&mut buf[..used]);
                }
                Some(msg) => msg,
            };

            let avail = buflen - used;

            if msg.len() <= avail {
                buf[used..][..msg.len()].copy_from_slice(&msg);
                used += msg.len();
            } else {
                let (now, later) = msg.split_at(avail);
                buf[used..].copy_from_slice(now);

                let mut hp = defmt::unwrap!(HEAP.try_lock());
                let mut habox = defmt::unwrap!(hp.alloc_box_array(0u8, later.len()).ok());
                habox.copy_from_slice(later);

                // Okay to ignore error - We just made space
                deq.push_front(habox).ok();

                used += avail;
            }
        }

        // if we've reached here, we've filled the destination buffer
        Ok(buf)
    }

    fn send<'a>(&mut self, port: u16, buf: &'a [u8]) -> Result<(), &'a [u8]> {
        // Check if port is mapped
        if !self.ports.contains_key(&port) {
            defmt::println!("Unregistered port: {=u16}", port);
            return Err(buf);
        }

        let mut remaining = buf;

        // We loop here, as the bbqueue may be in a "wraparound" situation,
        // where there is only a little space available at the "tail" of the
        // ring buffer, but there is space available at the front. This will
        // generally only execute once (no wraparound) or twice (some wraparound),
        // unless the driver clears some more space while we are processing.
        while !remaining.is_empty() {
            let rem_len = max_encoding_length(remaining.len());

            // Attempt to get a write grant to send to the driver...
            match self.out.grant_max_remaining(rem_len) {
                // Can we write the port and AT LEAST one byte of data
                // and a null terminator?
                Ok(wgr) if wgr.len() <= (2 + 1 + 1) => {
                    return Err(remaining);
                }

                // We have exhausted the available size in the outgoing buffer.
                // Give the user the remaining, unsent part, so they can try again
                // later.
                Err(bbqueue::Error::InsufficientSize) => {
                    return Err(remaining);
                },

                // We got some (or all) necessary space.
                // Copy the relevant data, and slide the window over.
                // (If this was "all", then `remaining` will be empty)
                Ok(mut wgr) => {
                    // We should take the lesser of:
                    //
                    // * The grant length, minus three overhead bytes (two for port,
                    //     one for sentinel), which is always positive due to check
                    //     above, OR
                    // * The remaining data length
                    let to_use = (wgr.len() - 4).min(remaining.len());
                    let (now, later) = remaining.split_at(to_use);

                    // Setup and encode the message
                    let msg = Message { port, data: now };

                    // This SHOULD never fail, make it an assert for now to catch dumb errors
                    let used = match msg.encode_to(&mut wgr) {
                        Ok(used) => used.len(),
                        Err(_) => {
                            defmt::println!("Encoding failure!");
                            defmt::println!("remaining len: {=usize}", remaining.len());
                            defmt::println!("wgr len: {=usize}", wgr.len());
                            defmt::println!("now len: {=usize}", now.len());
                            defmt::println!("remaining: {=[u8]}", remaining);
                            defmt::println!("now: {=[u8]}", now);
                            defmt::panic!();
                        },
                    };

                    // Commit the ENCODED number of bytes, and store the remaining
                    // UNENCODED bytes
                    wgr.commit(used);
                    remaining = later;
                },

                // This error case generally represents some kind of logic error
                // such as retaining a grant (our problem), or an internal fault
                // of bbqueue. Either way, this is not likely to be a recoverable
                // error. Until we have better fault recovery logic in place,
                // just panic and get it over with.
                Err(_e) => {
                    defmt::panic!("ERROR: USB UART Send!");
                }
            }
        }

        // This means that we reached `remaining.is_empty()`, and all
        // data has been successfully sent.
        Ok(())
    }
}

pub fn enable_usb_interrupts(usbd: &USBD) {
    usbd.intenset.write(|w| {
        // rg -o "events_[a-z_0-9]+" ./usbd.rs | sort | uniq
        w.endepin0().set_bit();
        w.endepin1().set_bit();
        w.endepin2().set_bit();
        w.endepin3().set_bit();
        w.endepin4().set_bit();
        w.endepin5().set_bit();
        w.endepin6().set_bit();
        w.endepin7().set_bit();

        w.endepout0().set_bit();
        w.endepout1().set_bit();
        w.endepout2().set_bit();
        w.endepout3().set_bit();
        w.endepout4().set_bit();
        w.endepout5().set_bit();
        w.endepout6().set_bit();
        w.endepout7().set_bit();

        w.ep0datadone().set_bit();
        w.ep0setup().set_bit();
        w.sof().set_bit();
        w.usbevent().set_bit();
        w.usbreset().set_bit();
        w
    });
}



struct Accumulator<const N: usize> {
    buf: [u8; N],
    idx: usize,
}

enum AccError<'a> {
    NoRoomNoRem,
    NoRoomWithRem(&'a [u8]),
}

impl<const N: usize> Accumulator<N> {
    fn new() -> Self {
        Self {
            buf: [0u8; N],
            idx: 0,
        }
    }
    fn feed<'a>(&mut self, buf: &'a [u8]) -> Result<Option<AccSuccess<'a, N>>, AccError<'a>> {
        match buf.iter().position(|b| *b == 0) {
            Some(n) if (self.idx + n) <= N => {
                let (now, later) = buf.split_at(n + 1);
                self.buf[self.idx..][..now.len()].copy_from_slice(now);
                let mut msg = AccMsg {
                    buf: [0u8; N],
                    len: self.idx + now.len(),
                };
                msg.buf[..msg.len].copy_from_slice(&self.buf[..msg.len]);
                self.idx = 0;
                Ok(Some(AccSuccess {
                    remainder: later,
                    msg,
                }))
            },
            Some(n) if n < buf.len() => {
                self.idx = 0;
                Err(AccError::NoRoomWithRem(&buf[(n + 1)..]))
            },
            Some(_) => {
                self.idx = 0;
                Err(AccError::NoRoomNoRem)
            }
            None if (self.idx + buf.len()) <= N => {
                self.buf[self.idx..][..buf.len()].copy_from_slice(buf);
                self.idx += buf.len();
                Ok(None)
            },
            None => {
                // No room, and no zero. Truncate the current buf.
                self.idx = 0;
                Err(AccError::NoRoomNoRem)
            },
        }
    }
}

struct AccSuccess<'a, const N: usize> {
    remainder: &'a [u8],
    msg: AccMsg<N>,
}

struct AccMsg<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> AccMsg<N> {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }
}
