use anyhow::{ensure, Result};
use ax5043::{config, config::*};
use ax5043::{registers::*, tui, Registers, RX, TX};
use clap::Parser;
use crc::{Crc, CRC_16_GENIBUS}; // TODO: this CRC works but is it correct?
use gpiod::{Chip, EdgeDetect, Options};
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use mio_signals::{Signal, Signals};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::{io::Write, os::fd::AsRawFd, time::Duration};
use timerfd::{SetTimeFlags, TimerFd, TimerState};

fn configure_radio(radio: &mut Registers) -> Result<(Board, ChannelParameters)> {
    let board = config::board::C3_LBAND.write(radio)?;
    let synth = config::synth::LBAND_DC_457.write(radio, &board)?;
    let channel = config::channel::GMSK_60000.write(radio, &board)?;

    radio.FIFOTHRESH().write(128)?; // Half the FIFO size

    synth.autorange(radio)?;
    Ok((board, channel))
}

/*
first SYNTHBOOST SYNTHSETTLE
second IFINIT COARSEAGC AGC RSSI

preamble1: PS0
    TMGRXPREAMBLE1 to reset to second?

preamble2: PS1
    MATCH1
    TMGRXPREAMBLE2

preamble3: PS2
    MATCH0
    TMGRXPREAMBLE3

packet: PS3
    SFD
*/

pub fn configure_radio_rx(radio: &mut Registers) -> Result<(Board, ChannelParameters)> {
    let (board, channel) = configure_radio(radio)?;

    radio.PERF_F18().write(0x02)?; // TODO set by radiolab during RX
    radio.PERF_F26().write(0x98)?;

    let rxp = RXParameters::MSK {
        max_dr_offset: 0, // TODO derived from what?
        freq_offs_corr: true,
        ampl_filter: 0,
        frequency_leak: 0,
    }
    .write(radio, &board, &channel)?;

    let set0 = RXParameterSet {
        //agc: RXParameterAGC::new(&board, &channel),
        agc: RXParameterAGC::radiolab(),
        gain: RXParameterGain {
            time_corr_frac: 4,
            datarate_corr_frac: 255,
            phase: 0b0011,
            filter: 0b11,
            baseband: Some(RXParameterFreq {
                phase: 0x06,
                freq: 0x06,
            }),
            rf: None,
            amplitude: 0b0110,
            deviation_update: true,
            ampl_agc_jump_correction: false,
            ampl_averaging: false,
        },
        freq_dev: None,
        decay: 0b0110,
        baseband_offset: RXParameterBasebandOffset { a: 0, b: 0 },
    };
    set0.write0(radio, &board, &channel, &rxp)?;

    let set1 = RXParameterSet {
        //agc: RXParameterAGC::new(&board, &channel),
        agc: RXParameterAGC::radiolab(),
        gain: RXParameterGain {
            time_corr_frac: 16,
            datarate_corr_frac: 512,
            phase: 0b0011,
            filter: 0b11,
            baseband: Some(RXParameterFreq {
                phase: 0x06,
                freq: 0x06,
            }),
            rf: None,
            amplitude: 0b0110,
            deviation_update: true,
            ampl_agc_jump_correction: false,
            ampl_averaging: false,
        },
        freq_dev: Some(0x32),
        decay: 0b0110,
        baseband_offset: RXParameterBasebandOffset { a: 0, b: 0 },
    };
    set1.write1(radio, &board, &channel, &rxp)?;

    let set3 = RXParameterSet {
        agc: RXParameterAGC::off(),
        gain: RXParameterGain {
            time_corr_frac: 32,
            datarate_corr_frac: 1024,
            phase: 0b0011,
            filter: 0b11,
            baseband: Some(RXParameterFreq {
                phase: 0x0A,
                freq: 0x0A,
            }),
            rf: None,
            amplitude: 0b0110,
            deviation_update: true,
            ampl_agc_jump_correction: false,
            ampl_averaging: false,
        },
        freq_dev: Some(0x32),
        decay: 0b0110,
        baseband_offset: RXParameterBasebandOffset { a: 0, b: 0 },
    };
    set3.write3(radio, &board, &channel, &rxp)?;

    // TODO: set timeout (TMGRXPREAMBLEx) off of expected bitrate + preamble length?
    RXParameterStages {
        preamble1: Some(Preamble1 {
            pattern: PatternMatch1 {
                pat: 0x7E7E,
                len: 15,
                raw: false,
                min: 0,
                max: 15,
            },
            //timeout: Float5 { m: 0x17, e: 5 },
            timeout: Float5 { m: 0, e: 0 },
            set: RxParamSet::Set0,
        }),
        preamble2: Some(Preamble2 {
            pattern: PatternMatch0 {
                pat: 0x7E7E_7E7E,
                len: 31,
                raw: false,
                min: 0,
                max: 31,
            },
            timeout: Float5 { m: 0x17, e: 5 },
            set: RxParamSet::Set1,
        }),
        preamble3: None,
        packet: RxParamSet::Set3,
    }
    .write(radio)?;

    radio.PKTMAXLEN().write(0xFF)?;
    radio.PKTLENCFG().write(PktLenCfg { pos: 0, bits: 0xF })?;
    radio.PKTLENOFFSET().write(0x09)?;

    radio.PKTCHUNKSIZE().write(0x09)?;
    radio.PKTACCEPTFLAGS().write(PktAcceptFlags::LRGP)?;

    radio.RSSIREFERENCE().write(32)?;

    Ok((board, channel))
}

fn process_chunk(chunk: FIFOChunkRX, packet: &mut Vec<u8>, uplink: &mut UdpSocket) -> Result<()> {
    if let FIFOChunkRX::DATA { flags, ref data } = chunk {
        //println!("{:02X?}", chunk);
        if flags.intersects(
            FIFODataRXFlags::ABORT
                | FIFODataRXFlags::SIZEFAIL
                | FIFODataRXFlags::ADDRFAIL
                | FIFODataRXFlags::CRCFAIL
                | FIFODataRXFlags::RESIDUE,
        ) {
            println!(
                "LBAND REJECTED {:?} {:02X?} ...+{}",
                flags,
                data[0],
                data.len()
            );
            packet.clear();
            return Ok(());
        }

        if flags.contains(FIFODataRXFlags::PKTSTART) {
            if !packet.is_empty() {
                println!(
                    "LBAND PKT RESTART rejecting {:02X?} ...+{}",
                    packet[0],
                    packet.len(),
                );
            }
            packet.clear();
        }

        if !flags.contains(FIFODataRXFlags::PKTSTART) && packet.is_empty() {
            println!("Invalid continued chunk {:02X?}", chunk);
            return Ok(());
        }

        packet.write_all(data)?;
        if flags.contains(FIFODataRXFlags::PKTEND) {
            let bytes = packet.split_off(packet.len() - 2);
            let checksum = u16::from_be_bytes([bytes[0], bytes[1]]);
            let ccitt = Crc::<u16>::new(&CRC_16_GENIBUS);
            let mut digest = ccitt.digest();
            digest.update(packet);
            let calculated = digest.finalize();

            if calculated == checksum {
                uplink.send(packet)?;
                println!("LBAND RX PACKET: {:02X?}", packet);
            } else {
                println!(
                    "Rejected CRC: received 0x{:x}, calculated 0x{:x}",
                    checksum, calculated
                );
            }
            packet.clear();
        }
    }
    Ok(())
}

fn read_packet(radio: &mut Registers, packet: &mut Vec<u8>, uplink: &mut UdpSocket) -> Result<()> {
    let len = radio.FIFOCOUNT().read()?;
    if len == 0 {
        return Ok(());
    }

    match radio.FIFODATARX().read(len.into()) {
        Ok(chunks) => {
            for chunk in chunks {
                process_chunk(chunk, packet, uplink)?;
            }
        }
        Err(e) => {
            // FIFO Errors are usually just overflow, non-fatal
            println!("{}", e);
            packet.clear();
        }
    }
    Ok(())
}

#[derive(Parser, Debug)]
/// Try it out: `socat UDP-LISTEN:10025 STDOUT`
struct Args {
    #[arg(short, long, default_value = "10025")]
    uplink: u16,
    #[arg(short, long, default_value = "/dev/spidev1.1")]
    spi: String,
    /// For example 10.18.17.6:10035
    #[arg(short, long)]
    telemetry: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut poll = Poll::new()?;
    let registry = poll.registry();
    let mut events = Events::with_capacity(128);

    let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.uplink);
    let mut uplink = UdpSocket::bind(src)?;
    uplink.connect(dest)?;

    let mut telemetry: Option<UdpSocket> = None;
    if let Some(addr) = args.telemetry {
        let dest: SocketAddr = addr.parse().unwrap();
        let socket = UdpSocket::bind(src)?;
        socket.connect(dest)?;
        telemetry = Some(socket);
    }

    const SIGINT: Token = Token(3);
    let mut signals = Signals::new(Signal::Interrupt.into())?;
    registry.register(&mut signals, SIGINT, Interest::READABLE)?;

    let chip = Chip::new("gpiochip1")?;
    let opts = Options::output([27]).values([false]);
    let pa_enable = chip.request_lines(opts)?;

    let chip0 = Chip::new("gpiochip0")?;
    let opts = Options::input([31]).edge(EdgeDetect::Rising);
    let mut lband_irq = chip0.request_lines(opts)?;

    const IRQ: Token = Token(4);
    registry.register(
        &mut SourceFd(&lband_irq.as_raw_fd()),
        IRQ,
        Interest::READABLE,
    )?;

    let mut tfd = TimerFd::new().unwrap();
    if telemetry.is_some() {
        tfd.set_state(
            TimerState::Periodic {
                current: Duration::new(1, 0),
                interval: Duration::from_millis(25),
            },
            SetTimeFlags::Default,
        );
    } else {
        tfd.set_state(
            TimerState::Periodic {
                current: Duration::new(0, 0),
                interval: Duration::new(0, 0),
            },
            SetTimeFlags::Default,
        );
    }
    const TELEMETRY: Token = Token(5);
    registry.register(
        &mut SourceFd(&tfd.as_raw_fd()),
        TELEMETRY,
        Interest::READABLE,
    )?;

    let spi0 = ax5043::open(args.spi)?;
    let mut status = ax5043::Status::empty();
    let mut callback = |_: &_, _, s, _: &_| {
        if s != status {
            if let Some(ref socket) = telemetry {
                tui::CommState::STATUS(s).send(socket).unwrap();
            }
            status = s;
        }
    };
    let mut radio = ax5043::Registers::new(spi0, &mut callback);
    radio.reset()?;

    let rev = radio.REVISION().read()?;
    ensure!(
        rev == 0x51,
        "Unexpected revision {}, expected {}",
        rev,
        0x51,
    );

    let (board, channel) = configure_radio_rx(&mut radio)?;
    pa_enable.set_values([true])?;

    if let Some(ref socket) = telemetry {
        tui::CommState::BOARD(board.clone()).send(socket)?;
        tui::CommState::REGISTERS(tui::StatusRegisters::new(&mut radio)?).send(socket)?;
        tui::CommState::CONFIG(tui::Config {
            rxparams: tui::RXParams::new(&mut radio, &board)?,
            set0: tui::RXParameterSet::set0(&mut radio)?,
            set1: tui::RXParameterSet::set1(&mut radio)?,
            set2: tui::RXParameterSet::set2(&mut radio)?,
            set3: tui::RXParameterSet::set3(&mut radio)?,
            synthesizer: tui::Synthesizer::new(&mut radio, &board)?,
            packet_controller: tui::PacketController::new(&mut radio)?,
            packet_format: tui::PacketFormat::new(&mut radio)?,
        })
        .send(socket)?;
    }
    radio.PWRMODE().write(PwrMode {
        flags: PwrFlags::XOEN | PwrFlags::REFEN,
        mode: PwrModes::RX,
    })?;

    _ = radio.PLLRANGINGA().read()?; // sticky lock bit ~ IRQPLLUNLIOCK, gate
    _ = radio.POWSTICKYSTAT().read()?; // clear sticky power flags for PWR_GOOD

    radio.FIFOCMD().write(FIFOCmd {
        mode: FIFOCmds::CLEAR_ERROR,
        auto_commit: false,
    })?;
    radio.FIFOCMD().write(FIFOCmd {
        mode: FIFOCmds::CLEAR_DATA,
        auto_commit: false,
    })?;

    radio
        .IRQMASK()
        .write(ax5043::registers::IRQ::FIFONOTEMPTY)?;
    let mut packet = Vec::new();

    'outer: loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                TELEMETRY => {
                    tfd.read();
                    if let Some(ref socket) = telemetry {
                        tui::CommState::STATE(tui::RXState::new(&mut radio, &channel)?)
                            .send(socket)?;
                        tui::CommState::REGISTERS(tui::StatusRegisters::new(&mut radio)?)
                            .send(socket)?;
                    }
                }
                IRQ => {
                    lband_irq.read_event()?;
                    while lband_irq.get_values(0u8)? > 0 {
                        read_packet(&mut radio, &mut packet, &mut uplink)?;
                    }
                }
                SIGINT => break 'outer,
                _ => unreachable!(),
            }
        }
    }

    pa_enable.set_values([false])?;
    radio.reset()?;
    Ok(())
}
