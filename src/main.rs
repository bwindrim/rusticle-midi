//! This example shows how to use the PIO module in the RP2040 chip to implement a duplex UART.
//! The PIO module is a very powerful peripheral that can be used to implement many different
//! protocols. It is a very flexible state machine that can be programmed to do almost anything.
//!
//! This example opens up a USB device that implements a CDC ACM serial port. It then uses the
//! PIO module to implement a UART that is connected to the USB serial port. This allows you to
//! communicate with a device connected to the RP2040 over USB serial.

#![no_std]
#![no_main]

use defmt::{info, panic, trace};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::join::{join, join3};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::pio_programs::uart::{PioUartRx, PioUartRxProgram, PioUartTx, PioUartTxProgram};
use embassy_rp::usb::{Driver, Instance, InterruptHandler};
use embassy_rp::{bind_interrupts, pio};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::class::midi::{MidiClass, Receiver, Sender};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};
use embedded_io_async::{Read, Write};
use panic_probe as _;

const BAUD_RATE: u32 = 31250; // standard MIDI baud rate
const MIDI_PACKET_SIZE: usize = 4; // 4 bytes per MIDI packet
// Queues of 16 3-byte MIDI messages.
static UART_IN_MIDI_OUT: Channel<ThreadModeRawMutex, [u8; MIDI_PACKET_SIZE], 16> = Channel::new();
static MIDI_IN_UART_OUT: Channel<ThreadModeRawMutex, [u8; MIDI_PACKET_SIZE], 16> = Channel::new();

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
});

#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(_spawner: Spawner) {
    info!("Hello there!");

    let p = embassy_rp::init(Default::default());

    // Create the driver, from the HAL.
    let driver = Driver::new(p.USB, Irqs);

    // Create embassy-usb Config
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("Rustical USB-MIDI interface");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Create embassy-usb DeviceBuilder using the driver and config.
    // It needs some buffers for building the descriptors.
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 64];

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [], // no msos descriptors
        &mut control_buf,
    );

    // Creates class using the builder.
    // The default configuration returns basic setup with 1 IN and 1 OUT jack.
    let midi = MidiClass::new(
        &mut builder,
        1,  // number of input jacks
        1,  // number of output jacks
        64, // max packet size for the endpoints
    );

    // Build the builder.
    let mut usb = builder.build();

    // Run the USB device.
    let usb_fut = usb.run();

    let (mut midi_send, mut midi_recv) = midi.split();

    // Use the Midi class.
    let midi_to_uart_fut = async {
        loop {
            midi_recv.wait_connection().await;
            info!("MIDI recv connected");
            let _ = midi_echo_in(&mut midi_recv).await;
            info!("MIDI recv disconnected");
        }
    };

    let uart_to_midi_fut = async {
        loop {
            midi_send.wait_connection().await;
            info!("MIDI send connected");
            let _ = midi_echo_out(&mut midi_send).await;
            info!("MIDI send disconnected");
        }
    };

    // Make a future that runs both the MIDI to UART and UART to MIDI tasks concurrently.
    let midi_fut = join(midi_to_uart_fut, uart_to_midi_fut);

    // PIO setup for UART.
    let pio::Pio {
        mut common,
        sm0,
        sm1,
        ..
    } = pio::Pio::new(p.PIO0, Irqs);
    // Create the PIO UART programs for TX and RX.
    let tx_program = PioUartTxProgram::new(&mut common);
    let rx_program = PioUartRxProgram::new(&mut common);
    // Create the PIO UART TX and RX instances.
    let mut uart_tx = PioUartTx::new(BAUD_RATE, &mut common, sm0, p.PIN_14, &tx_program);
    let mut uart_rx = PioUartRx::new(BAUD_RATE, &mut common, sm1, p.PIN_6, &rx_program);

    // Make a future that runs both the UART read and write tasks concurrently.
    let uart_future = join(uart_read(&mut uart_rx), uart_write(&mut uart_tx));

    // Run everything concurrently.
    join3(usb_fut, midi_fut, uart_future).await;
}

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

/// Read from the UART and write it to the MIDI IN channel.
async fn uart_read<PIO: pio::Instance, const SM: usize>(uart_rx: &mut PioUartRx<'_, PIO, SM>) -> ! {
    loop {
        let byte = uart_rx.read_u8().await;
        let packet = [0x0F, byte, 0, 0];
        trace!("UART IN: {:x}", packet);
        UART_IN_MIDI_OUT.send(packet).await;
    }
}

/// Read from the UART TX pipe and write it to the UART
async fn uart_write<PIO: pio::Instance, const SM: usize>(
    uart_tx: &mut PioUartTx<'_, PIO, SM>,
) -> ! {
    loop {
        let data = MIDI_IN_UART_OUT.receive().await;
        trace!("UART OUT: {:x}", data);
        uart_tx.write(&data[1..]).await;
    }
}

async fn midi_echo_in<'d, T: Instance + 'd>(
    midi: &mut Receiver<'d, Driver<'d, T>>,
) -> Result<(), Disconnected> {
    let mut buf = [0; 64];
    loop {
        let n = midi.read_packet(&mut buf).await?;
        let data = &buf[..n];
        info!("data: {:x}", data);

        for packet in buf[..n].chunks_exact(4) {
            info!("packet: {:x}", packet);
            let uart_packet: [u8; MIDI_PACKET_SIZE] = packet[..4].try_into().unwrap();
            MIDI_IN_UART_OUT.send(uart_packet).await;
        }
    }
}

async fn midi_echo_out<'d, T: Instance + 'd>(
    midi: &mut Sender<'d, Driver<'d, T>>,
) -> Result<(), Disconnected> {
    loop {
        let packet = UART_IN_MIDI_OUT.receive().await;
        info!("packet: {:x}", packet);
        midi.write_packet(&packet).await?;
    }
}
