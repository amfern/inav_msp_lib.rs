extern crate alloc;
extern crate multiwii_serial_protocol;
extern crate serialport;
extern crate packed_struct;
#[macro_use]
extern crate packed_struct_codegen;

use multiwii_serial_protocol::{MspCommandCode, MspPacket, MspPacketDirection, MspParser};
use serialport::SerialPort;
use packed_struct::prelude::*;

use async_std::sync::{channel, Arc, Mutex, Sender, Receiver};
use async_std::{io, task};
use async_std::future;

use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};


// TODO: move this to multiwii_serial_protocol.rs library
// TODO: and figure out why we can't call unpack on structs from multiwii library
#[derive(PackedStruct, Debug, Copy, Clone)]
#[packed_struct(bytes = "6", endian = "lsb", bit_numbering = "msb0")]
pub struct MspDataFlashRead {
    pub read_address: u32,
    pub read_length: u16,
}

pub struct MspDataFlashReply {
    pub read_address: u32,
    pub payload: Vec<u8>,
}

#[derive(PackedStruct, Debug, Copy, Clone)]
#[packed_struct(bytes = "1", endian = "lsb", bit_numbering = "msb0")]
pub struct MspDataFlashSummaryReply {
    #[packed_field(bits = "6")]
    pub supported: bool,
    #[packed_field(bits = "7")]
    pub ready: bool,
    pub sectors: u32,
    pub total_size_bytes: u32,
    pub used_size_bytes: u32,
}

#[derive(PackedStruct, Debug, Copy, Clone)]
#[packed_struct(bytes = "4", endian = "lsb", bit_numbering = "msb0")]
pub struct MspModeRange {
    pub box_id: u8,
    pub aux_channel_index: u8,
    pub start_step: u8,
    pub end_step: u8,
}

#[derive(PackedStruct, Debug, Copy, Clone)]
#[packed_struct(bytes = "5", endian = "lsb", bit_numbering = "msb0")]
pub struct MspSetModeRange {
    pub index: u8,
    #[packed_field(size_bytes="4")]
    pub mode_range: MspModeRange,
}

// const MAX_MODE_ACTIVATION_CONDITION_COUNT: u8 = 20u8;

#[derive(PackedStruct, Debug, Copy, Clone)]
#[packed_struct(endian = "lsb", bit_numbering = "msb0")]
pub struct MspModeRangesReplay {
    #[packed_field(element_size_bytes="4")]
    mode_ranges: [MspModeRange; 20], // 20 is defined as MAX_MODE_ACTIVATION_CONDITION_COUNT
}


#[derive(Debug)]
pub struct ModeRange {
    pub index: u8,
    pub box_id: u8,
    pub aux_channel_index: u8,
    pub start_step: u8,
    pub end_step: u8,
}

// TODO: extract this code to rust module(different file)

pub struct FlashDataFile {
    chunk_recv: Receiver<MspDataFlashReply>,
    msp_writer_send: Sender<MspPacket>,
    parser_locked: Arc<Mutex<MspParser>>,
    used_size: u32,
    next_address: u32,
    // requested_address: u32,
    received_address: u32,
}

// TODO: we should return interface that implements async_std::io::Read trait
// TODO: why not return move the payload vec instead of the io result??
impl FlashDataFile {
    pub async fn read_chunk(&mut self) -> io::Result<Vec<u8>> {
        if self.received_address >= self.used_size {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "use after close"));
        }

        loop {
            if self.next_address > self.received_address || self.next_address == 0 {
                let payload = MspDataFlashRead {
                    read_address: self.next_address,
                    read_length: 0x1000,
                };
                let packed = payload.pack();

                let packet = multiwii_serial_protocol::MspPacket {
                    cmd: multiwii_serial_protocol::MspCommandCode::MSP_DATAFLASH_READ as u16,
                    direction: multiwii_serial_protocol::MspPacketDirection::ToFlightController,
                    data: packed.to_vec(),
                };

                self.msp_writer_send.send(packet).await;
            }

            let timeout_res = future::timeout(Duration::from_millis(50), self.chunk_recv.recv()).await;

            // resend the packet
            if timeout_res.is_ok() {
                match timeout_res.unwrap() {
                    None => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "device disconnected")),
                    Some(packet) => {

                        if packet.read_address >= self.next_address {
                            self.received_address = packet.read_address;
                            self.next_address = packet.read_address + packet.payload.len() as u32;
                        } else {
                            continue;
                        }

                        println!("{:?}/{:?}", packet.read_address, self.used_size);

                        if self.received_address >= self.used_size {
                            return Ok(vec![]);
                        }

                        return Ok(packet.payload);
                    }
                }
            } else {
                (*self.parser_locked.lock().await).reset();
            }
        }
    }
}

pub struct INavMsp {
    parser_locked: Arc<Mutex<MspParser>>,

    msp_reader_send: Sender<MspPacket>,
    msp_reader_recv: Receiver<MspPacket>,
    msp_writer_send: Sender<MspPacket>,
    msp_writer_recv: Receiver<MspPacket>,

    mode_ranges_recv: Receiver<MspModeRangesReplay>,
    mode_ranges_send: Sender<MspModeRangesReplay>,
    set_mode_range_ack_recv: Receiver<()>,
    set_mode_range_ack_send: Sender<()>,
    summary_recv: Receiver<MspDataFlashSummaryReply>,
    summary_send: Sender<MspDataFlashSummaryReply>,
    chunk_recv: Receiver<MspDataFlashReply>,
    chunk_send: Sender<MspDataFlashReply>,
}

impl INavMsp {
    // Create a new parserSerialPort
    pub fn new() -> INavMsp {
        let (msp_reader_send, msp_reader_recv) = channel::<MspPacket>(1);
        let (msp_writer_send, msp_writer_recv) = channel::<MspPacket>(1);

        let (mode_ranges_send, mode_ranges_recv) = channel::<MspModeRangesReplay>(1);
        let (set_mode_range_ack_send, set_mode_range_ack_recv) = channel::<()>(1);
        let (summary_send, summary_recv) = channel::<MspDataFlashSummaryReply>(1);
        let (chunk_send, chunk_recv) = channel::<MspDataFlashReply>(1);

        let parser = MspParser::new();
        let parser_locked = Arc::new(Mutex::new(parser));

        return INavMsp {
            parser_locked: parser_locked,
            msp_reader_send: msp_reader_send,
            msp_reader_recv: msp_reader_recv,
            msp_writer_send: msp_writer_send,
            msp_writer_recv: msp_writer_recv,

            mode_ranges_send: mode_ranges_send,
            mode_ranges_recv: mode_ranges_recv,
            set_mode_range_ack_recv: set_mode_range_ack_recv,
            set_mode_range_ack_send: set_mode_range_ack_send,
            summary_send: summary_send,
            summary_recv: summary_recv,
            chunk_send: chunk_send,
            chunk_recv: chunk_recv,
        };
	  }

    pub fn start(&self, serial: Box<dyn SerialPort>) {
        let serial_clone = serial.try_clone().unwrap();

        INavMsp::process_input(serial, self.parser_locked.clone(), self.msp_reader_send.clone());
        INavMsp::process_output(serial_clone, self.msp_writer_recv.clone());
        INavMsp::process_route(
            self.msp_reader_recv.clone(),
            self.mode_ranges_send.clone(),
            self.set_mode_range_ack_send.clone(),
            self.summary_send.clone(),
            self.chunk_send.clone(),
        );
    }

    fn process_route(
        msp_reader_recv: Receiver<MspPacket>,
        mode_ranges_send: Sender<MspModeRangesReplay>,
        set_mode_range_ack_send: Sender<()>,
        summary_send: Sender<MspDataFlashSummaryReply>,
        chunk_send: Sender<MspDataFlashReply>,
    ) {
        task::spawn(async move {
            loop {
                let packet = match msp_reader_recv.recv().await {
                    None => break,
                    Some(packet) => packet,
                };

                if packet.direction != MspPacketDirection::FromFlightController {
                    continue;
                }

                if packet.cmd == MspCommandCode::MSP_MODE_RANGES as u16 {
                    let ranges = MspModeRangesReplay::unpack_from_slice(&packet.data).unwrap();
                    mode_ranges_send.send(ranges).await;
                }

                if packet.cmd == MspCommandCode::MSP_SET_MODE_RANGE as u16 {
                    // packet data should be empty, so just signal ack is received
                    set_mode_range_ack_send.send(()).await;
                }

                if packet.cmd == MspCommandCode::MSP_DATAFLASH_SUMMARY as u16 {
                    let summary = MspDataFlashSummaryReply::unpack_from_slice(&packet.data).unwrap();
                    summary_send.send(summary).await;
                }

                if packet.cmd == MspCommandCode::MSP_DATAFLASH_READ as u16 {
                    // extract the read address from the packet
                    let mut s = [0; 4];
                    s.copy_from_slice(&packet.data[..4]);
                    let packet_address = u32::from_le_bytes(s);

                    // remove the last address bytes and send to remaning payload to file stream(stdout)
                    let packet_payload = &packet.data[4..];

                    let chunk = MspDataFlashReply {
                        read_address: packet_address,
                        payload: packet_payload.to_vec(),
                    };
                    chunk_send.send(chunk).await;
                }

                // TODO: create debug flag for additional print on demand
                // println!("{:?}", packet);
            }
        });
    }

    // TODO: return joinhandler, so we can stop the tasks on drop
    fn process_input(
        mut serial: Box<dyn SerialPort>,
        parser_locked: Arc<Mutex<MspParser>>,
        msp_reader_send: Sender<MspPacket>
    ) -> Arc<AtomicBool> {
        let should_stop = Arc::new(AtomicBool::new(false));
        let should_stop_clone = should_stop.clone();

        // task 1: read into input channel from serial(reading from serial is blocking)
        task::spawn(async move {
            while should_stop.load(Ordering::Relaxed) == false {
                let mut serial_buf: Vec<u8> = vec![0; 1000];
                match serial.read(serial_buf.as_mut_slice()) {
                    Ok(bytes) => {
                        for n in 0..bytes {
                            match (*parser_locked.lock().await).parse(serial_buf[n]) {
                                Ok(Some(p)) => {
                                    msp_reader_send.send(p).await
                                },
                                Err(e) => eprintln!("bad crc {:?}", e),
                                Ok(None) => ()
                            }
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::TimedOut => task::yield_now().await,
                    Err(e) => eprintln!("{:?}", e),
                }
            }
        });
        return should_stop_clone;
	  }

    // TODO: return joinhandler, so we can stop the tasks on drop
    fn process_output(
        mut serial: Box<dyn SerialPort>,
        msp_writer_recv: Receiver<MspPacket>,
    ) {
        task::spawn(async move {
            loop {
                let packet = match msp_writer_recv.recv().await {
                    None => break,
                    Some(packet) => packet,
                };

                let size = packet.packet_size_bytes_v2();
                let mut output = vec![0; size];

                packet
                    .serialize_v2(&mut output)
                    .expect("Failed to serialize");

                // because inav doesn't support uart flow control, we simply try write untill success
                loop {
                    match serial.write(&output) {
                        Ok(_) => break,
                        Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                            // controller is busy/serial buffer is full, sleep and attempt write again
                            task::sleep(Duration::from_millis(1)).await;
                        }
                        Err(e) => eprintln!("failed to write{:?}", e),
                    }
                }
            }
        });
	  }

    // TODO: because this is a serial protocol, we cannot allow two reads of the file at the same time.
    //       so throw error, if this function is called while another file is open already
    pub async fn open_flash_data(&self) -> FlashDataFile {
        // await for summary
        let summary = self.flash_summary().await;
        let used_size = summary.unwrap().used_size_bytes;

        return FlashDataFile {
            chunk_recv: self.chunk_recv.clone(),
            msp_writer_send: self.msp_writer_send.clone(),
            parser_locked: self.parser_locked.clone(),
            used_size: used_size,
            next_address: 0u32,
            received_address: 0u32,
        };
	  }

    pub async fn flash_summary(&self) -> io::Result<MspDataFlashSummaryReply> {
        let packet = MspPacket {
            cmd: MspCommandCode::MSP_DATAFLASH_SUMMARY as u16,
            direction: MspPacketDirection::ToFlightController,
            data: vec![],
        };

        self.msp_writer_send.send(packet).await;

        let timeout_res = future::timeout(Duration::from_millis(30), self.summary_recv.recv()).await;
        if timeout_res.is_ok() {
            return Ok(timeout_res.unwrap().unwrap());
        }

        return Err(io::Error::new(io::ErrorKind::TimedOut, "timedout waiting for summary response"));
	  }

    pub async fn set_mode_range(&self, mode: ModeRange) -> io::Result<()> {

        let payload = MspSetModeRange {
            index: mode.index,
            mode_range: MspModeRange {
                box_id: mode.box_id,
                aux_channel_index: mode.aux_channel_index,
                start_step: mode.start_step,
                end_step: mode.end_step,
            }
        };

        let packet = MspPacket {
            cmd: MspCommandCode::MSP_SET_MODE_RANGE as u16,
            direction: MspPacketDirection::ToFlightController,
            data: payload.pack().to_vec(),
        };

        self.msp_writer_send.send(packet).await;

        // TODO: we are not sure this ack is for our request, because there is no id for the request
        let timeout_res = future::timeout(Duration::from_millis(30), self.set_mode_range_ack_recv.recv()).await;
        if timeout_res.is_ok() {
            return Ok(timeout_res.unwrap().unwrap());
        }

        return Err(io::Error::new(io::ErrorKind::TimedOut, "timedout waiting for set mode range response"));
	  }

    pub async fn get_mode_ranges(&self) -> io::Result<Vec<ModeRange>> {
        let packet = MspPacket {
            cmd: MspCommandCode::MSP_MODE_RANGES as u16,
            direction: MspPacketDirection::ToFlightController,
            data: vec![],
        };

        self.msp_writer_send.send(packet).await;

        // TODO: we are not sure this ack is for our request, because there is no id for the request
        // TODO: what if we are reading packet that was sent long time ago
        // TODO: also currently if no one is reading the channges, we may hang

        let timeout_res = future::timeout(Duration::from_millis(30), self.mode_ranges_recv.recv()).await;
        if !timeout_res.is_ok() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timedout waiting for set mode range response"));
        }

        let ranges_replay = timeout_res.unwrap().unwrap();
        let mut valid_ranges = vec![];

        // TODO: not all 20 ranges will be active, return only the active ranges
        ranges_replay.mode_ranges.iter().enumerate().fold(&mut valid_ranges, |acc, (i, r)| {
            if r.start_step != 0 && r.end_step != 0 {
                acc.push(ModeRange {
                    index: i as u8,
                    box_id: r.box_id,
                    aux_channel_index: r.aux_channel_index,
                    start_step: r.start_step,
                    end_step: r.end_step,
                });
            }

            return acc;
        });

        return Ok(valid_ranges);
	  }

}
