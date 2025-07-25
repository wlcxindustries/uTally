use bondrewd::Bitfields;
use bytemuck::Zeroable;
use defmt::{debug, error, panic, warn};
use embassy_futures::select::{select4, Either4};
use embassy_net::driver::LinkState;
use embassy_net_driver_channel::{self as ch};
use embassy_time::{Duration, Ticker, Timer};
use embedded_hal::{
    digital::OutputPin,
    spi::{self, ErrorKind},
};
use embedded_hal_async::{
    digital::Wait,
    spi::{Operation, SpiDevice},
};
use embedded_registers::spi::{CodecAsync, SpiDeviceAsync};
use embedded_registers::{Register, RegisterInterfaceAsync};
use registers::*;

mod registers;

const MTU: usize = 1514;

#[repr(u8)]
enum Opcode {
    RegRead = 0b00,
    RegWrite = 0b01,
    RXRead = 0b10,
    TXWrite = 0b11,
}

const CHIP_ID_FAMILY: u8 = 0x88;
const CHIP_ID_CHIP: u8 = 0x7;

struct Codec {}

impl CodecAsync for Codec {
    async fn read_register<R, I>(interface: &mut I) -> Result<R, I::Error>
    where
        R: embedded_registers::ReadableRegister,
        I: embedded_hal_async::spi::r#SpiDevice,
    {
        let mut reg = R::zeroed();
        interface
            .transaction(&mut [
                Operation::Write(&reg_cmd(
                    Opcode::RegRead,
                    R::ADDRESS.try_into().unwrap(),
                    R::REGISTER_SIZE.try_into().unwrap(),
                )),
                Operation::Read(reg.data_mut()),
            ])
            .await?;
        Ok(reg)
    }

    async fn write_register<R, I>(
        interface: &mut I,
        register: impl AsRef<R>,
    ) -> Result<(), I::Error>
    where
        R: embedded_registers::WritableRegister,
        I: embedded_hal_async::spi::r#SpiDevice,
    {
        interface
            .transaction(&mut [
                Operation::Write(&reg_cmd(
                    Opcode::RegWrite,
                    R::ADDRESS.try_into().unwrap(),
                    R::REGISTER_SIZE.try_into().unwrap(),
                )),
                Operation::Write(register.as_ref().data()),
            ])
            .await
    }
}

fn reg_cmd(o: Opcode, addr: u8, count: u8) -> [u8; 2] {
    // The device only supports accessing 4-aligned addresses, with selectable bytes
    // being read/written ("byte enables").
    let byte_enable = match (addr & 0b11, count) {
        (0, 2) => 0b0011,
        (2, 2) => 0b1100,
        (_, _) => unimplemented!(),
    };
    [
        ((o as u8) << 6) | (byte_enable << 2) | (addr >> 6),
        (addr & 0b00111100) << 2,
    ]
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    SpiError(ErrorKind),
    BadChipId {
        expected_family: u8,
        actual_family: u8,
        expected_chip: u8,
        actual_chip: u8,
    },
    FailedBuiltInSelfTest {
        rx_bist_failed: bool,
        tx_bist_failed: bool,
    },
    TxPacketTooBig {
        size: usize,
        max: u16,
    },
    RxFrameInvalid,
    RxNoFrameAvailable,
}

impl<SE: spi::Error> From<SE> for Error {
    fn from(value: SE) -> Self {
        Self::SpiError(value.kind())
    }
}

/// Driver runner.
///
/// `.run()` must be called in a dedicated task for the driver to, well, run.
pub struct Runner<'d, SPI, INT: Wait, RST: OutputPin>
where
    SPI: SpiDevice,
{
    chip: Chip<SPI>,
    ch: ch::Runner<'d, MTU>,
    int: INT,
    rst: RST,
}

struct Chip<SPI: SpiDevice> {
    dev: SpiDeviceAsync<SPI, Codec>,
    next_frame_id: u8,
    last_unacked_id: u8,
}

#[derive(Bitfields, Default)]
#[bondrewd(reverse, enforce_bytes = 2)]
pub struct TXCtrlWord {
    transmit_interrupt_on_completion: bool,
    #[bondrewd(bit_length = 9, endianness = "be")]
    __: u16,
    #[bondrewd(bit_length = 6)]
    frame_id: u8,
}

impl<SPI: SpiDevice> Chip<SPI> {
    async fn init(&mut self) -> Result<(), Error> {
        self.dev
            .write_register(GRR::zeroed().with_global_soft_reset(true))
            .await?;
        Timer::after_millis(10).await;
        self.dev.write_register(GRR::zeroed()).await?;
        Timer::after_millis(10).await;
        let cider = self.dev.read_register::<CIDER>().await?;
        defmt::println!("{:?}", cider);
        if cider.read_chip_id() != CHIP_ID_CHIP || cider.read_family_id() != CHIP_ID_FAMILY {
            return Err(Error::BadChipId {
                expected_family: CHIP_ID_FAMILY,
                actual_family: cider.read_family_id(),
                expected_chip: CHIP_ID_CHIP,
                actual_chip: cider.read_chip_id(),
            });
        }
        #[cfg(feature = "defmt")]
        defmt::info!("Found ksz8851snl rev {}", cider.read_revision_id());
        let mbir = self.dev.read_register::<MBIR>().await?;
        if mbir.read_rx_memory_bist_fail() || mbir.read_tx_memory_bist_fail() {
            return Err(Error::FailedBuiltInSelfTest {
                rx_bist_failed: mbir.read_rx_memory_bist_fail(),
                tx_bist_failed: mbir.read_tx_memory_bist_fail(),
            });
        }

        let txfdpr = self
            .dev
            .read_register::<TXFDPR>()
            .await
            .unwrap()
            .with_tx_frame_data_pointer_auto_increment(true);
        self.dev.write_register(txfdpr).await.unwrap();

        let txcr = self
            .dev
            .read_register::<TXCR>()
            .await
            .unwrap()
            .with_checksum_gen_icmp(false)
            .with_checksum_gen_tcp(false)
            .with_checksum_gen_ip(false)
            .with_flow_control_enable(false)
            .with_padding_enable(true)
            .with_crc_enable(true);
        self.dev.write_register(txcr).await.unwrap();

        // Configure rx interrupt to be every 10ms at most. TODO: is this sufficient?
        self.dev
            .write_register(RXDTTR::zeroed().with_receive_duration_timer_threshold(1000))
            .await
            .unwrap();

        let rxfdpr = self
            .dev
            .read_register::<RXFDPR>()
            .await
            .unwrap()
            .with_rx_frame_pointer_auto_increment(true);
        self.dev.write_register(rxfdpr).await.unwrap();

        // let rxfctr = self
        //     .dev
        //     .read_register::<RXFCTR>()
        //     .await
        //     .unwrap()
        //     .with_recieve_frame_count_threshold(1);
        // self.dev.write_register(rxfctr).await.unwrap();
        let rxqcr = self
            .dev
            .read_register::<RXQCR>()
            .await
            .unwrap()
            //.with_rx_frame_cound_threshold_enable(true)
            .with_rx_duration_timer_threshold_enable(true)
            .with_rx_ip_header_two_byte_offset_enable(false)
            .with_auto_dequeue_rxq_frame_enable(true);
        self.dev.write_register(rxqcr).await.unwrap();

        let rxcr = self
            .dev
            .read_register::<RXCR1>()
            .await
            .unwrap()
            .with_receive_udp_frame_checksum_check_enable(false)
            .with_receive_tcp_frame_checksum_check_enable(false)
            .with_receive_ip_frame_checksum_check_enable(false)
            .with_receive_flow_control_enable(false)
            .with_receive_broadcast_enable(false)
            .with_receive_unicast_enable(true);
        self.dev.write_register(rxcr).await.unwrap();

        let rxcr2 = self
            .dev
            .read_register::<RXCR2>()
            .await
            .unwrap()
            .with_ip4_ip6_udp_fragment_frame_pass(true)
            .with_receive_ip4_ip6_udp_frame_checksum_equal_zero(true)
            .with_udp_lite_frame_enable(true)
            .with_receive_icmp_frame_checksum_check_enable(true)
            .with_spi_receive_data_burst_length(SPIRxDataBurstLength::SINGLEFRAME);
        self.dev.write_register(rxcr2).await.unwrap();

        let ier = self
            .dev
            .read_register::<IER>()
            .await
            .unwrap()
            .with_link_change_enable(true)
            .with_transmit_space_available_enable(true)
            .with_transmit_enable(true)
            .with_receive_enable(true)
            .with_receive_overrun_enable(true)
            .with_spi_bus_error_enable(true);
        self.dev.write_register(ier).await.unwrap();

        let p1cr = self
            .dev
            .read_register::<P1CR>()
            .await
            .unwrap()
            .with_led_off(true);
        self.dev.write_register(p1cr).await.unwrap();

        // There are two ways to transmit - auto enqueue and manual enqueue.
        // Auto enqueue involves setting TXQCR[2] at init time, and means you can (supposedly)
        // write multiple frames at once.
        // Manual enqueue involves setting TXQCR[0] *after* you've written the frame to transmit.
        let txqcr = self
            .dev
            .read_register::<TXQCR>()
            .await
            .unwrap()
            .with_auto_enqueue_txq_frame_enable(false);
        self.dev.write_register(txqcr).await.unwrap();

        let txcr = self
            .dev
            .read_register::<TXCR>()
            .await
            .unwrap()
            .with_transmit_enable(true);
        self.dev.write_register(txcr).await.unwrap();

        let rxcr = self
            .dev
            .read_register::<RXCR1>()
            .await
            .unwrap()
            .with_receive_enable(true);
        self.dev.write_register(rxcr).await.unwrap();

        Ok(())
    }

    async fn set_mac(&mut self, mac_addr: [u8; 6]) -> Result<(), Error> {
        self.dev
            .write_register(MARH::zeroed().with_marh(mac_addr[0..=1].try_into().unwrap()))
            .await?;
        self.dev
            .write_register(MARM::zeroed().with_marm(mac_addr[2..=3].try_into().unwrap()))
            .await?;
        self.dev
            .write_register(MARL::zeroed().with_marl(mac_addr[4..=5].try_into().unwrap()))
            .await?;
        Ok(())
    }

    async fn get_mac(&mut self) -> Result<[u8; 6], Error> {
        let high = self.dev.read_register::<MARH>().await?.read_marh();
        let med = self.dev.read_register::<MARM>().await?.read_marm();
        let low = self.dev.read_register::<MARL>().await?.read_marl();
        Ok([high[0], high[1], med[0], med[1], low[0], low[1]])
    }

    async fn link_state(&mut self) -> Result<LinkState, Error> {
        Ok(
            if self.dev.read_register::<P1SR>().await?.read_link_good() {
                LinkState::Up
            } else {
                LinkState::Down
            },
        )
    }

    /// Check if the chip has space in the tx buffer to tx a packet of len `tx_len`.
    /// returns true if there's enough space, false if not. If not, also enables the
    /// chip's memory available interrupt so we're informed when there is space.
    async fn ready_tx(&mut self, tx_len: usize) -> Result<bool, Error> {
        if tx_len > 2000 {
            return Err(Error::TxPacketTooBig {
                size: tx_len,
                max: 2000,
            });
        }
        let available = self
            .dev
            .read_register::<TXMIR>()
            .await
            .unwrap()
            .read_txma_memory_available();
        if (tx_len + 4) > available.into() {
            // No room in the device's buffer currently
            self.dev
                .write_register(
                    TXNTFSR::zeroed().with_tx_next_total_frame_size((tx_len + 4) as u16),
                )
                .await?;
            self.dev
                .write_register(TXQCR::zeroed().with_txq_memory_available_monitor(true))
                .await?;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// TX the given frame immediately. This assumes that we know there's enough space in
    /// the chip's tx buffer by calling having called `ready_tx` already.
    async fn tx(&mut self, buf: &[u8]) -> Result<(), Error> {
        // Disable interrupts
        let ier = self.dev.read_register::<IER>().await.unwrap();
        self.dev.write_register(IER::zeroed()).await.unwrap();
        // Enable TXQ write access
        let mut rxqcr = self.dev.read_register::<RXQCR>().await.unwrap();
        rxqcr.write_start_dma_access(true);
        self.dev.write_register(rxqcr).await.unwrap();

        let byte_count: [u8; 2] = (buf.len() as u16).to_le_bytes();

        let mut txc = TXCtrlWord::default();
        txc.transmit_interrupt_on_completion = true;
        txc.frame_id = self.next_frame_id;

        let _pad = (4 - (buf.len() % 4)) % 4;
        let pad = &mut [0u8; 3][0.._pad];

        self.dev
            .interface
            .transaction(&mut [
                Operation::Write(&[(Opcode::TXWrite as u8) << 6]),
                Operation::Write(&txc.into_bytes()),
                Operation::Write(&byte_count),
                Operation::Write(buf),
                Operation::Write(pad),
            ])
            .await?;
        if self.next_frame_id == 0x1f {
            self.next_frame_id = 0;
        } else {
            self.next_frame_id += 1;
        }

        // Disable TXQ write access
        let mut rxqcr = self.dev.read_register::<RXQCR>().await.unwrap();
        rxqcr.write_start_dma_access(false);
        self.dev.write_register(rxqcr).await.unwrap();

        // Manually enqueue the frame
        let mut txqcr = self.dev.read_register::<TXQCR>().await.unwrap();
        txqcr.write_manual_enqueue_txq_frame_enable(true);
        self.dev.write_register(txqcr).await.unwrap();

        // Reenable interrupts
        self.dev.write_register(ier).await.unwrap();

        Ok(())
    }

    async fn rx_frames_available(&mut self) -> Result<u8, Error> {
        let fc = self
            .dev
            .read_register::<RXFCTR>()
            .await?
            .read_rx_frame_count();
        debug!("Chip reports {} frames available", fc);
        Ok(fc)
    }

    /// Receive a single frame from the chip.
    async fn rx(&mut self, rx_buf: &mut [u8]) -> Result<usize, Error> {
        // Disable interrupts
        let ier = self.dev.read_register::<IER>().await?;
        assert!(!ier.read_receive_enable());
        self.dev.write_register(IER::zeroed()).await.unwrap();

        let frame_status = self.dev.read_register::<RXFHSR>().await?.read_all();
        let byte_count = self
            .dev
            .read_register::<RXFHBCR>()
            .await
            .unwrap()
            .read_receive_byte_count();
        debug!("frame RX, {} bytes, {}", byte_count, frame_status);
        if !frame_status.frame_valid {
            // Either there is no frame or it's not done receiving.
            return Err(Error::RxNoFrameAvailable);
        }
        if frame_status.crc_error
            || frame_status.runt_frame
            || frame_status.frame_too_long
            || frame_status.mii_error
            || frame_status.udp_checksum_status
            || frame_status.tcp_checksum_status
            || frame_status.ip_checksum_status
            || frame_status.icmp_checksum_status
        {
            // Frame error - discard
            let rxqcr = self
                .dev
                .read_register::<RXQCR>()
                .await
                .unwrap()
                .with_release_rx_error_frame(true);
            self.dev.write_register(rxqcr).await.unwrap();
            return Err(Error::RxFrameInvalid);
        }
        if usize::from(byte_count) > rx_buf.len() {
            panic!("RX byte count too big!!!");
        }

        // Reset the rx frame pointer
        let rxfdpr = self.dev.read_register::<RXFDPR>().await.unwrap();
        self.dev
            .write_register(rxfdpr.with_rx_frame_pointer(0))
            .await
            .unwrap();

        // Enable DMA
        let rxqcr = self
            .dev
            .read_register::<RXQCR>()
            .await
            .unwrap()
            .with_start_dma_access(true);
        self.dev.write_register(rxqcr).await.unwrap();

        // We need to read a multiple of 4 bytes in total - so we may need some padding
        let pad = (4 - (byte_count % 4)) % 4;
        let discard = &mut [0u8; 3];

        let mut status = RXFHSR::zeroed();
        let mut bc = RXFHBCR::zeroed();

        let crc = &mut [0u8; 4];

        self.dev
            .interface
            .transaction(&mut [
                Operation::Write(&[(Opcode::RXRead as u8) << 6]),
                Operation::Read(&mut [0u8; 4]),
                Operation::Read(status.data_mut()),
                Operation::Read(bc.data_mut()),
                Operation::Read(&mut rx_buf[0..(byte_count - 4) as usize]),
                Operation::Read(crc),
                Operation::Read(&mut discard[0..pad as usize]),
            ])
            .await
            .unwrap();

        debug!("Got frame with CRC {:x}", u32::from_be_bytes(*crc));

        assert_eq!(frame_status, status.read_all());
        assert_eq!(byte_count, bc.read_receive_byte_count());

        // Disable DMA
        let rxqcr = self
            .dev
            .read_register::<RXQCR>()
            .await
            .unwrap()
            .with_start_dma_access(false);
        self.dev.write_register(rxqcr).await.unwrap();

        // Reenable interrupts
        self.dev.write_register(ier).await.unwrap();

        Ok((byte_count - 4).into())
    }
}

impl<SPI: SpiDevice, INT: Wait, RST: OutputPin> Runner<'_, SPI, INT, RST> {
    pub async fn run(mut self) -> ! {
        let (state_ch, mut rx_ch, mut tx_ch) = self.ch.split();
        let mut tick = Ticker::every(Duration::from_millis(1000));
        // Set to false when the chip has reported it doesn't have enough space to tx the next
        // frame, then wait for the transmit_space_available interrupt.
        let mut tx_space_available = true;
        // Set to false after txing, while waiting for the transmit interrupt
        let mut tx_done = true;
        // Set to true when the receive interrupt has triggered.
        let mut rx_pending = 0;
        loop {
            match select4(
                self.int.wait_for_low(),
                async {
                    if tx_space_available && tx_done {
                        tx_ch.tx_buf().await
                    } else {
                        core::future::pending().await
                    }
                },
                async {
                    if rx_pending > 0 {
                        rx_ch.rx_buf().await
                    } else {
                        core::future::pending().await
                    }
                },
                tick.next(),
            )
            .await
            {
                Either4::First(_) => {
                    // Chip interrupted us - but why?
                    let isr = self
                        .chip
                        .dev
                        .read_register::<ISR>()
                        .await
                        .unwrap()
                        .read_all();
                    let mut isr_clear = ISR::zeroed();
                    if isr.link_change {
                        debug!("ISR: chip reports link state change");
                        state_ch.set_link_state(self.chip.link_state().await.unwrap());
                        isr_clear.write_link_change(true);
                    }
                    if isr.transmit {
                        debug!("ISR: chip reports frame transmitted");
                        isr_clear.write_transmit(true);
                        tx_done = true;
                    }
                    if isr.spi_bus_error {
                        debug!("ISR: chip reports spi bus error");
                        isr_clear.write_spi_bus_error(true);
                    }
                    if isr.receive_overrun {
                        panic!("REceive overrun :(((");
                    }
                    if isr.receive {
                        // Disable further receive interrupts
                        let ier = self
                            .chip
                            .dev
                            .read_register::<IER>()
                            .await
                            .unwrap()
                            .with_receive_enable(false);
                        self.chip.dev.write_register(ier).await.unwrap();
                        isr_clear.write_receive(true);
                    }
                    if isr.transmit_space_available {
                        debug!("ISR: chip reports transmit space available!");
                    }
                    // Clear the interrupts flags that we've processed
                    self.chip.dev.write_register(isr_clear).await.unwrap();

                    // Get received frame count. We have to do this *AFTER* clearing the interrupt
                    // as it is only then that the chip updates the frame counter register!
                    if isr.receive {
                        if rx_pending != 0 {
                            error!(
                                "Got new RX interrupt but rx_pending == {}. Ignoring",
                                rx_pending
                            );
                            continue;
                        }
                        let available = self.chip.rx_frames_available().await.unwrap();
                        debug!("ISR: chip reports {} packets received", available);
                        if rx_pending != 0 {
                            error!(
                                "Got new RX interrupt but rx_pending == {}. Ignoring",
                                rx_pending
                            );
                        } else {
                            rx_pending = available;
                        }
                    }
                }
                Either4::Second(p) => {
                    // TX
                    debug!("txing {} bytes", p.len());
                    // debug!("{:x}", p);
                    if self.chip.ready_tx(p.len()).await.unwrap() {
                        debug!("ready to tx");
                        self.chip.tx(p).await.unwrap();
                        tx_ch.tx_done();
                        tx_done = false; // Wait for the interrupt before txing any more frames
                    } else {
                        debug!("Chip says no space available");
                        tx_space_available = false;
                    }
                }
                Either4::Third(rx_buf) => {
                    match self.chip.rx(rx_buf).await {
                        Ok(len) => {
                            rx_ch.rx_done(len);
                            rx_pending -= 1;
                        }
                        Err(Error::RxFrameInvalid) => {
                            // TODO: unsure if errored frames count...
                            warn!("Invalid frame...");
                            rx_pending -= 1;
                        }
                        Err(Error::RxNoFrameAvailable) => {
                            warn!(
                                "rx pending {} but no frame was ready. resetting.",
                                rx_pending
                            );
                            rx_pending = 0;
                        }
                        Err(e) => {
                            panic!("{:?}", e);
                        }
                    }
                    if rx_pending == 0 {
                        // If we're received everything from the last RX interrupt, reenable it
                        debug!("Reenabling rx interrupt");
                        let ier = self
                            .chip
                            .dev
                            .read_register::<IER>()
                            .await
                            .unwrap()
                            .with_receive_enable(true);
                        self.chip.dev.write_register(ier).await.unwrap();
                    }
                }
                Either4::Fourth(()) => {
                    // Periodically update the link state in case we missed an interrupt
                    // somehow
                    state_ch.set_link_state(self.chip.link_state().await.unwrap());
                }
            }
        }
    }
}

pub type Device<'d> = embassy_net_driver_channel::Device<'d, MTU>;

pub struct State<const N_RX: usize, const N_TX: usize> {
    ch_state: ch::State<MTU, N_RX, N_TX>,
}

impl<const N_RX: usize, const N_TX: usize> State<N_TX, N_RX> {
    pub const fn new() -> Self {
        Self {
            ch_state: ch::State::new(),
        }
    }
}

pub async fn new<
    'a,
    const N_RX: usize,
    const N_TX: usize,
    SPI: SpiDevice,
    INT: Wait,
    RST: OutputPin,
>(
    mac_addr: [u8; 6],
    state: &'a mut State<N_RX, N_TX>,
    spi: SPI,
    int: INT,
    mut rst: RST,
) -> Result<(Device<'a>, Runner<'a, SPI, INT, RST>), Error> {
    rst.set_high().ok();
    Timer::after(Duration::from_millis(10)).await;

    let mut chip = Chip {
        dev: SpiDeviceAsync::new(spi),
        next_frame_id: 0,
        last_unacked_id: 0,
    };
    chip.init().await?;
    chip.set_mac(mac_addr).await?;
    let (runner, device) = ch::new(
        &mut state.ch_state,
        ch::driver::HardwareAddress::Ethernet(mac_addr),
    );
    Ok((
        device,
        Runner {
            ch: runner,
            chip,
            int,
            rst,
        },
    ))
}
