#![no_main]
#![no_std]

static DEFAULT_IMAGE: &[u8] = include_bytes!("../appbins/blinker.bin");

// use cmsis_dsp_sys as _;

const SINE_TABLE: [i16; 256] = [
    0, 804, 1608, 2410, 3212, 4011, 4808, 5602, 6393, 7179, 7962, 8739, 9512, 10278, 11039, 11793,
    12539, 13279, 14010, 14732, 15446, 16151, 16846, 17530, 18204, 18868, 19519, 20159, 20787,
    21403, 22005, 22594, 23170, 23731, 24279, 24811, 25329, 25832, 26319, 26790, 27245, 27683,
    28105, 28510, 28898, 29268, 29621, 29956, 30273, 30571, 30852, 31113, 31356, 31580, 31785,
    31971, 32137, 32285, 32412, 32521, 32609, 32678, 32728, 32757, 32767, 32757, 32728, 32678,
    32609, 32521, 32412, 32285, 32137, 31971, 31785, 31580, 31356, 31113, 30852, 30571, 30273,
    29956, 29621, 29268, 28898, 28510, 28105, 27683, 27245, 26790, 26319, 25832, 25329, 24811,
    24279, 23731, 23170, 22594, 22005, 21403, 20787, 20159, 19519, 18868, 18204, 17530, 16846,
    16151, 15446, 14732, 14010, 13279, 12539, 11793, 11039, 10278, 9512, 8739, 7962, 7179, 6393,
    5602, 4808, 4011, 3212, 2410, 1608, 804, 0, -804, -1608, -2410, -3212, -4011, -4808, -5602,
    -6393, -7179, -7962, -8739, -9512, -10278, -11039, -11793, -12539, -13279, -14010, -14732,
    -15446, -16151, -16846, -17530, -18204, -18868, -19519, -20159, -20787, -21403, -22005, -22594,
    -23170, -23731, -24279, -24811, -25329, -25832, -26319, -26790, -27245, -27683, -28105, -28510,
    -28898, -29268, -29621, -29956, -30273, -30571, -30852, -31113, -31356, -31580, -31785, -31971,
    -32137, -32285, -32412, -32521, -32609, -32678, -32728, -32757, -32767, -32757, -32728, -32678,
    -32609, -32521, -32412, -32285, -32137, -31971, -31785, -31580, -31356, -31113, -30852, -30571,
    -30273, -29956, -29621, -29268, -28898, -28510, -28105, -27683, -27245, -26790, -26319, -25832,
    -25329, -24811, -24279, -23731, -23170, -22594, -22005, -21403, -20787, -20159, -19519, -18868,
    -18204, -17530, -16846, -16151, -15446, -14732, -14010, -13279, -12539, -11793, -11039, -10278,
    -9512, -8739, -7962, -7179, -6393, -5602, -4808, -4011, -3212, -2410, -1608, -804,
];

#[rtic::app(device = nrf52840_hal::pac, dispatchers = [SWI0_EGU0])]
mod app {
    use super::{letsago, DEFAULT_IMAGE};
    use core::sync::atomic::Ordering;
    use cortex_m::singleton;
    use defmt::unwrap;
    use groundhog::RollingTimer;
    use groundhog_nrf52::GlobalRollingTimer;
    use kernel::{
        alloc::HEAP,
        drivers::{
            nrf52_pin::MPin,
            usb_serial::{enable_usb_interrupts, setup_usb_uart, UsbUartIsr, UsbUartParts},
        },
        loader::validate_header,
        monotonic::MonoTimer,
        syscall::{syscall_clear, try_recv_syscall},
        traits::{BlockStorage, GpioPin},
    };
    use nrf52840_hal::{
        clocks::{ExternalOscillator, Internal, LfOscStopped},
        gpio::Level,
        pac::{GPIOTE, TIMER0},
        ppi::ConfigurablePpi,
        prelude::Ppi,
        usbd::{UsbPeripheral, Usbd},
        Clocks,
    };
    use usb_device::{
        class_prelude::UsbBusAllocator,
        device::{UsbDeviceBuilder, UsbVidPid},
    };
    use usbd_serial::{SerialPort, USB_CLASS_CDC};

    #[monotonic(binds = TIMER0, default = true)]
    type Monotonic = MonoTimer<TIMER0>;

    #[shared]
    struct Shared {
        spi: kernel::drivers::nrf52_spim_nonblocking::Spim,
    }

    #[local]
    struct Local {
        usb_isr: UsbUartIsr,
        machine: kernel::traits::Machine,
        rng: nrf52840_hal::Rng,
        ppi0: nrf52840_hal::ppi::Ppi0,
        // prog_loaded: Option<(*const u8, usize)>,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        let device = cx.device;

        // Setup clocks early in the process. We need this for USB later
        let clocks = Clocks::new(device.CLOCK);
        let clocks = clocks.enable_ext_hfosc();
        let clocks =
            unwrap!(singleton!(: Clocks<ExternalOscillator, Internal, LfOscStopped> = clocks));

        // Enable instruction caches for MAXIMUM SPEED
        device.NVMC.icachecnf.write(|w| w.cacheen().set_bit());
        cortex_m::asm::isb();

        // Configure the monotonic timer, currently using TIMER0, a 32-bit, 1MHz timer
        let mono = Monotonic::new(device.TIMER0);

        // I am annoying, and prefer my own libraries.
        GlobalRollingTimer::init(device.TIMER1);

        let rng = nrf52840_hal::Rng::new(device.RNG);

        // Setup the heap
        HEAP.init().ok();

        // Reset the syscall contents
        syscall_clear();

        // Before we give away the USB peripheral, enable the relevant interrupts
        enable_usb_interrupts(&device.USBD);

        let (usb_dev, usb_serial) = {
            let usb_bus = Usbd::new(UsbPeripheral::new(device.USBD, clocks));
            let usb_bus =
                defmt::unwrap!(singleton!(:UsbBusAllocator<Usbd<UsbPeripheral>> = usb_bus));

            let usb_serial = SerialPort::new(usb_bus);
            let usb_dev = UsbDeviceBuilder::new(usb_bus, UsbVidPid(0x16c0, 0x27dd))
                .manufacturer("OVAR Labs")
                .product("Anachro Pellegrino")
                // TODO: Use some kind of unique ID. This will probably require another singleton,
                // as the storage must be static. Probably heapless::String -> singleton!()
                .serial_number("ajm001")
                .device_class(USB_CLASS_CDC)
                .max_packet_size_0(64) // (makes control transfers 8x faster)
                .build();

            (usb_dev, usb_serial)
        };

        let UsbUartParts { isr, sys } = defmt::unwrap!(setup_usb_uart(usb_dev, usb_serial));

        let pins = kernel::map_pins(device.P0, device.P1);
        let qsp = kernel::qspi::QspiPins {
            qspi_copi_io0: pins.qspi_d0.degrade(),
            qspi_cipo_io1: pins.qspi_d1.degrade(),
            qspi_io2: pins.qspi_d2.degrade(),
            qspi_io3: pins.qspi_d3.degrade(),
            qspi_csn: pins.qspi_csn.degrade(),
            qspi_sck: pins.qspi_sck.degrade(),
        };
        let qspi = kernel::qspi::Qspi::new(device.QSPI, qsp);
        let mut block = defmt::unwrap!(kernel::drivers::gd25q16::Gd25q16::new(qspi));

        let prog_loaded = if let Some(blk) = kernel::MAGIC_BOOT.read_clear() {
            unsafe {
                extern "C" {
                    static _app_start: u32;
                    static _app_len: u32;
                }
                defmt::println!("Told to boot block {=u32}!", blk);
                let app_start = (&_app_start) as *const u32 as *const u8 as *mut u8;
                let app_len = (&_app_len) as *const u32 as usize;
                block.block_load_to(blk, app_start, app_len).ok()
            }
        } else {
            None
        };

        let mut hg = defmt::unwrap!(HEAP.try_lock());

        let to_uart: &'static mut dyn kernel::traits::Serial = defmt::unwrap!(hg.leak_send(sys));
        let to_block: &'static mut dyn kernel::traits::BlockStorage =
            defmt::unwrap!(hg.leak_send(block));

        //
        // Map GPIO pins
        //

        // LEDs
        let led1 = defmt::unwrap!(hg.leak_send(MPin::new(pins.led1.degrade())));
        let led2 = defmt::unwrap!(hg.leak_send(MPin::new(pins.led2.degrade())));

        // IRQ/AUX pins
        let d_dreq_pre = pins.d09.degrade().into_floating_input();

        // Enable hi-to-low interrupts
        let gpiote = nrf52840_hal::gpiote::Gpiote::new(device.GPIOTE);
        let ch0 = gpiote.channel0();
        let ch_ev = ch0.input_pin(&d_dreq_pre);
        ch_ev.hi_to_lo();

        let ppi = nrf52840_hal::ppi::Parts::new(device.PPI);
        let mut ppi0 = ppi.ppi0;

        ppi0.set_event_endpoint(ch0.event());
        ppi0.set_task_endpoint(&device.SPIM3.tasks_stop);
        ppi0.disable();

        let d_dreq = defmt::unwrap!(hg.leak_send(MPin::new_input_floating(d_dreq_pre)));
        // let d06 = defmt::unwrap!(hg.leak_send(MPin::new(pins.d06.degrade())));
        let scl = defmt::unwrap!(hg.leak_send(MPin::new(pins.scl.degrade())));
        let sda = defmt::unwrap!(hg.leak_send(MPin::new(pins.sda.degrade())));

        let array_gpios: [&'static mut dyn GpioPin; 5] = [led1, led2, d_dreq, scl, sda];
        let leak_gpios = defmt::unwrap!(hg.leak_send(array_gpios));

        // Chip Selects
        let d11 =
            defmt::unwrap!(hg.leak_send(pins.d11.degrade().into_push_pull_output(Level::High)));
        let d05 =
            defmt::unwrap!(hg.leak_send(pins.d05.degrade().into_push_pull_output(Level::High)));
        let d_xcs =
            defmt::unwrap!(hg.leak_send(pins.d06.degrade().into_push_pull_output(Level::High)));
        let d12 =
            defmt::unwrap!(hg.leak_send(pins.d12.degrade().into_push_pull_output(Level::High)));
        let d13 =
            defmt::unwrap!(hg.leak_send(pins.d13.degrade().into_push_pull_output(Level::High)));
        let d_xdcs =
            defmt::unwrap!(hg.leak_send(pins.d10.degrade().into_push_pull_output(Level::High)));

        let csn_pins: [&'static mut dyn kernel::traits::OutputPin; 6] = [
            d11, d05, d_xcs, d12, d13, d_xdcs, // TODO: Oops
        ];
        let leak_csns = defmt::unwrap!(hg.leak_send(csn_pins));

        let spi = kernel::drivers::nrf52_spim_nonblocking::Spim::new(
            device.SPIM3,
            kernel::drivers::nrf52_spim_nonblocking::Pins {
                sck: pins.sclk.into_push_pull_output(Level::Low).degrade(),
                mosi: Some(pins.mosi.into_push_pull_output(Level::Low).degrade()),
                miso: Some(pins.miso.into_floating_input().degrade()),
            },
            nrf52840_hal::spim::Frequency::M1,
            embedded_hal::spi::MODE_0,
            0x00,
            leak_csns,
        );
        // let spi: &'static mut dyn kernel::traits::Spi = defmt::unwrap!(hg.leak_send(spi));

        let machine = kernel::traits::Machine {
            serial: to_uart,
            block_storage: Some(to_block),
            // spi: Some(spi),
            spi: None,
            gpios: leak_gpios.as_mut_slice(),
        };

        (
            Shared { spi },
            Local {
                usb_isr: isr,
                machine,
                rng,
                ppi0,
                // prog_loaded,
            },
            init::Monotonics(mono),
        )
    }

    // #[task(binds = SVCall, local = [machine], priority = 1)]
    // fn svc(cx: svc::Context) {
    //     let machine = cx.local.machine;

    //     if let Ok(()) = try_recv_syscall(|req| {
    //         machine.handle_syscall(req)
    //     }) {
    //         // defmt::println!("Handled syscall!");
    //     }
    // }

    #[task(binds = SPIM3, shared = [spi], priority = 2)]
    fn spim3(mut cx: spim3::Context) {
        // TODO: NOT this
        let gpiote = unsafe { &*GPIOTE::ptr() };

        // Clear channel 0 events (which probably stopped our SPI device)
        gpiote.events_in[0].write(|w| w);

        // defmt::println!("[INT]: SPIM3");

        cx.shared.spi.lock(|spi| {
            spi.end_send();
        })
    }

    #[task(binds = USBD, local = [usb_isr], priority = 3)]
    fn usb_tick(cx: usb_tick::Context) {
        cx.local.usb_isr.poll();
    }

    // TODO: I am currently polling the syscall interfaces in the idle function,
    // since I don't have syscalls yet. In the future, the `machine` will be given
    // to the SWI handler, and idle will basically just launch a program. I think.
    // Maybe idle will use SWIs too.
    // #[idle(local = [prog_loaded])]
    #[idle(local = [machine, rng, ppi0], shared = [spi])]
    fn idle(mut cx: idle::Context) -> ! {
        use common::syscall::request::GpioMode;

        // let freq = cx.local.rng.random_u8();
        // let freq = (freq as f32) + 256.0;
        let freq = 441.0f32;

        defmt::println!("Hello, world!");

        let timer = GlobalRollingTimer::default();
        let start = timer.get_ticks();

        // Wait, to allow RTT to attach
        while timer.millis_since(start) < 1000 {}

        const CSN_XCS: u8 = 2;
        const CSN_XDCS: u8 = 5;
        const IRQ_DREQ: usize = 2;

        let machine = cx.local.machine;
        let dreq = &mut machine.gpios[IRQ_DREQ];
        dreq.set_mode(GpioMode::InputFloating).unwrap();

        // let spi = machine.spi.as_mut().unwrap();

        // SCI command goes:
        // Operation: 1 byte
        //     * Read:  0x03
        //     * Write: 0x02
        // Address: 1 byte
        // Data: 2 bytes

        // Wait for DREQ to go high
        loop {
            match dreq.read_pin() {
                Ok(true) => break,
                Ok(false) => {}
                Err(()) => panic!(),
            }
        }

        // SOFT RESET
        let mut buf_out = HEAP.try_lock().unwrap().alloc_box_array(0u8, 4).unwrap();
        buf_out.copy_from_slice(&[
            0x02, // Write
            0x00, // MODE
            0x48, 0x04,
        ]);
        cx.shared
            .spi
            .lock(|spi| spi.send(CSN_XCS, 1_000, buf_out).map_err(drop).unwrap());

        // Wait "a couple hundred cycles", I dunno, 5ms?
        let delay = timer.get_ticks();
        while timer.millis_since(delay) < 5 {}

        // Wait for DREQ to go high
        loop {
            match dreq.read_pin() {
                Ok(true) => break,
                Ok(false) => {}
                Err(()) => panic!(),
            }
        }

        // Set CLOCKF register (0x03)
        // 10.2 recommend a value of 9800, meaning
        // 100 - 11 - 00000000000
        //   XTALIx3.5 (Mult)
        //   XTALIx1.5 (Max boost)
        //   Freq = 0 (12.288MHz)
        let mut buf_out = HEAP.try_lock().unwrap().alloc_box_array(0u8, 4).unwrap();
        buf_out.copy_from_slice(&[
            0x02, // Write
            0x03, // CLOCKF
            0x98, 0x00,
        ]);
        cx.shared
            .spi
            .lock(|spi| spi.send(CSN_XCS, 1_000, buf_out).map_err(drop).unwrap());

        // Wait "a couple hundred cycles", I dunno, 5ms?
        let delay = timer.get_ticks();
        while timer.millis_since(delay) < 5 {}

        // One bit every 4 CLKI pulses.
        // Since we've increased the clock rate to
        // 3.5xXTALI (~43MHz), that gives us a max SPI
        // clock rate of ~10.75MHz. Use 8MHz.

        // Before decoding, set
        // * SCI_MODE
        // * SCI_BASS
        // * SCI_CLOCKF (done)
        // * SCI_VOL

        // Probably skip the others, but probably set volume to like 0x2424,
        // which means -18.0dB in each ear.
        let mut buf_out = HEAP.try_lock().unwrap().alloc_box_array(0u8, 4).unwrap();
        buf_out.copy_from_slice(&[
            0x02, // Write
            0x0B, // VOLUME
            0x24, 0x24,
        ]);
        cx.shared
            .spi
            .lock(|spi| spi.send(CSN_XCS, 1_000, buf_out).map_err(drop).unwrap());

        // Wait "a couple hundred cycles", I dunno, 5ms?
        let delay = timer.get_ticks();
        while timer.millis_since(delay) < 5 {}
        let mut idata = [0i16; 200];

        defmt::println!("Generating data...");
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        let t0 = timer.get_ticks();

        {
            use crate::SINE_TABLE;

            let samp_per_cyc: f32 = 44100.0 / freq; // 141.7
            let fincr = 256.0 / samp_per_cyc; // 1.81
            let incr: i32 = (((1 << 24) as f32) * fincr) as i32;

            // generate the next 256 samples...
            let mut cur_offset = 0i32;

            idata.chunks_exact_mut(2).for_each(|i| {
                let val = cur_offset as u32;
                let idx_now = ((val >> 24) & 0xFF) as u8;
                let idx_nxt = idx_now.wrapping_add(1);
                let base_val = SINE_TABLE[idx_now as usize] as i32;
                let next_val = SINE_TABLE[idx_nxt as usize] as i32;

                // Distance to next value - perform 256 slot linear interpolation
                let off = ((val >> 16) & 0xFF) as i32; // 0..=255
                let cur_weight = base_val.wrapping_mul(256i32.wrapping_sub(off));
                let nxt_weight = next_val.wrapping_mul(off);
                let ttl_weight = cur_weight.wrapping_add(nxt_weight);
                let ttl_val = ttl_weight >> 8; // div 256
                let ttl_val = ttl_val as i16;

                // Set the linearly interpolated value
                i.iter_mut().for_each(|i| *i = ttl_val);

                cur_offset = cur_offset.wrapping_add(incr);
            });
        }

        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        let elapsed = timer.ticks_since(t0);
        defmt::println!("Took {=u32} ticks", elapsed);

        // Example: A 44100 Hz 16-bit stereo PCM header would read as follows:
        // 0000 52 49 46 46 ff ff ff ff 57 41 56 45 66 6d 74 20 |RIFF....WAVEfmt |
        // 0100 10 00 00 00 01 00 02 00 44 ac 00 00 10 b1 02 00 |........D.......|
        // 0200 04 00 10 00 64 61 74 61 ff ff ff ff             |....data....|

        let mut header = HEAP.try_lock().unwrap().alloc_box_array(0u8, 44).unwrap();
        header.copy_from_slice(&[
            0x52, 0x49, 0x46, 0x46, 0xff, 0xff, 0xff, 0xff, 0x57, 0x41, 0x56, 0x45, 0x66, 0x6d,
            0x74, 0x20, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x44, 0xac, 0x00, 0x00,
            0x10, 0xb1, 0x02, 0x00, 0x04, 0x00, 0x10, 0x00, 0x64, 0x61, 0x74, 0x61, 0xff, 0xff,
            0xff, 0xff,
        ]);
        cx.shared
            .spi
            .lock(|spi| spi.send(CSN_XDCS, 8_000, header).map_err(drop).unwrap());

        cx.local.ppi0.enable();
        let mut forever = idata.iter().cycle();
        let mut already_calcd = None;
        let mut iters = 0;
        while iters < 1000 {
            let small_buf = match already_calcd.take() {
                Some(sb) => sb,
                None => {
                    let mut small_buf =
                        HEAP.try_lock().unwrap().alloc_box_array(0u8, 2048).unwrap();
                    small_buf.chunks_exact_mut(2).for_each(|ch| {
                        ch.copy_from_slice(&forever.next().unwrap().to_le_bytes());
                    });
                    small_buf
                }
            };

            // Wait for DREQ to go high
            loop {
                match dreq.read_pin() {
                    Ok(true) => break,
                    _ => {}
                }
            }

            // TODO: this will be bad, but okay.
            cx.shared.spi.lock(|spi| {
                already_calcd = match spi.send(CSN_XDCS, 8_000, small_buf) {
                    Ok(()) => {
                        iters += 1;
                        None
                    }
                    Err(e) => {
                        spi.start_send();
                        Some(e)
                    }
                };
            });

            let start = timer.get_ticks();
            while timer.millis_since(start) <= 1 {}

            // cx.shared.spi.lock(|spi| {
            //     spi.start_send();
            // });
        }
        let start = timer.get_ticks();
        while timer.millis_since(start) <= 1000 {}
        kernel::exit();
    }
}

use core::arch::asm;
use cortex_m::register::{control, psp};

#[inline(always)]
unsafe fn letsago(sp: u32, entry: u32) -> ! {
    // Do the not-so-dangerous stuff in Rust.

    // Calculate the desired CONTROL register value.
    let mut cur_ctl = control::read();
    cur_ctl.set_npriv(control::Npriv::Unprivileged);
    cur_ctl.set_spsel(control::Spsel::Psp);
    let cur_ctl = cur_ctl.bits();

    // Write the PSP. Note: This won't take effect until after we write control.
    psp::write(sp);

    // Here's where the spooky stuff happens.
    asm!(
        // Write the CONTROL register, disabling privilege and enabling the PSP
        "msr CONTROL, {}",

        // Writing the CONTROL register means we need to emit an isb instruction
        "isb",

        // Branch directly to the loaded program. No coming back.
        "bx {}",
        in(reg) cur_ctl,
        in(reg) entry,
        options(noreturn, nomem, nostack),
    );
}

fn armsin(v: f32) -> f32 {
    extern "C" {
        fn arm_sin_f32(i: f32) -> f32;
    }
    unsafe { arm_sin_f32(v) }
}
