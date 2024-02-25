use rand::prelude::*;
use std::collections::VecDeque;
use std::fmt::format;
use std::io::{self, BufRead, Stdin, StdinLock};
use std::net::UdpSocket;
use std::collections::HashMap;
use std::time::Instant;
use std::time::Duration;

use crate::{read_to_string, safe_increment};
use crate::util::tcp_header::TcpHeader;

#[derive(Debug)]
enum Status {
    StandBy,
    Handshake,
    Sending,
    Finished,
}

#[derive(Clone, Debug)]
struct Packet {
    timestamp: Instant,
    data: Vec<u8>,
    seq_num: u32,
    ack_num: u32,
    confirm_ack: u32,
    data_len: u16
}

#[derive(Debug)]
pub struct Sender {
    remote_host: String,
    remote_port: u16,
    local_host: String,
    local_port: u16,
    status: Status,
    seq_num: u32,
    ack_num: u32,
    expect_seq: VecDeque<u32>,
    expect_ack: VecDeque<u32>,
    data: VecDeque<String>,
    init_seq: u32,
    socket: UdpSocket,
    rto: u64,
    in_flight: VecDeque<Packet>,
    wnd_size: u16,
    cur_wnd: u16,
    file: String,
    cur_buf: u16
}

impl Sender {
    pub fn new(remote_host: String, remote_port: u16, local_host: String) -> Result<Self, String> {
        let mut rng = rand::thread_rng();
        let seq_num: u32 = rng.gen();

        let socket = UdpSocket::bind(format!("{}:{}", local_host, 0)).map_err(|e| format!("{} -> Failed to bind to {}:{}", e, local_host, 0))?;
        let local = socket.local_addr().map_err(|e| format!("{e} -> Failed to get local port"))?;
        socket.set_nonblocking(true).map_err(|e| format!("{e} -> Failed to switch to non-blocking mode"))?;

        Ok(Sender {
            remote_host,
            remote_port,
            local_host,
            local_port: local.port(),
            status: Status::StandBy,
            init_seq: seq_num,
            seq_num,
            ack_num: 0,
            expect_seq: VecDeque::new(),
            expect_ack: VecDeque::new(),
            data: VecDeque::new(),
            socket,
            rto: 1000,
            in_flight: VecDeque::new(),
            wnd_size: 7000,
            cur_wnd: 7000,
            file: String::new(),
            cur_buf: 0
        })
    }

    pub fn start(&mut self) -> Result<(), String> {
        loop {
            match self.status {
                Status::StandBy => {
                    let mut buffer = String::new();
                    let stdin = io::stdin();
                    let mut handle = stdin.lock();
                    handle.read_line(&mut buffer).map_err(|e| format!("{e} -> Failed to read stdin"))?;

                    self.data = buffer.as_bytes().chunks(1460).map(| ch| String::from_utf8_lossy(ch).to_string()).collect();
                    self.status = Status::Handshake;
                }
                Status::Handshake => {
                    let header = TcpHeader {
                        source_port: self.local_port,
                        destination_port: self.remote_port,
                        sequence_number: self.seq_num,
                        ack_number: self.ack_num,
                        header_length: 4,
                        flags: 0b0000_0010,
                        window_size: self.wnd_size
                    };
                    // self.socket.send_to(&header.as_bytes(), format!("{}:{}", self.remote_host, self.remote_port)).map_err(|e| format!("{e} -> Failed to send SYN packet"))?;
                    // self.expect_ack.push_back(safe_increment(self.seq_num, 1));

                    self.register_packet(header, "");

                    dbg!(&self.seq_num);
                    

                    let mut buf: [u8; 1500] = [0;1500];
                    loop {
                        
                        match self.socket.recv(&mut buf) {
                            Ok(_) => {
                                let header = TcpHeader::new(&buf[..16]);
                                buf.fill(0);

                                if header.ack_number != self.in_flight[0].confirm_ack {
                                    continue;
                                }

                                if header.flags != 18 {
                                    continue;
                                }

                                if header.source_port != self.remote_port {
                                    continue;
                                }

                                if header.destination_port != self.local_port {
                                    continue;
                                }

                                self.cur_wnd = self.wnd_size.min(header.window_size);
                                self.in_flight.pop_front();
                                self.ack_num = safe_increment(header.sequence_number, 1);

                                let header = TcpHeader {
                                    source_port: self.local_port,
                                    destination_port: self.remote_port,
                                    sequence_number: self.seq_num,
                                    ack_number: self.ack_num,
                                    header_length: 4,
                                    flags: 0b0001_0000,
                                    window_size: self.wnd_size
                                };

                                dbg!(&self.seq_num);

                                self.register_packet(header, "");
                                self.status = Status::Sending;
                                break;
                            }
                            Err(_) => {}
                        }

                        self.check_retransmission();

                    }
                    
                }
                Status::Sending => {
                    while !self.in_flight.is_empty() {
                        self.check_retransmission();

                        let mut buf: [u8; 1500] = [0;1500];
                        match self.socket.recv(&mut buf) {
                            Ok(_) => {
                                let header = TcpHeader::new(&buf[..16]);

                                if header.flags != 8 {
                                    continue;
                                }

                                if header.source_port != self.remote_port {
                                    continue;
                                }

                                if header.destination_port != self.local_port {
                                    continue;
                                }

                                self.cur_wnd = self.wnd_size.min(header.window_size);

                                match Self::find_packet_index(&self.in_flight, header.ack_number) {
                                    Ok(ind) => {
                                        for _ in 0..=ind {
                                            self.in_flight.pop_front();
                                        }
                                    }
                                    Err(_) => {}
                                }

                                let fragment = read_to_string(&buf[16..]);
                                self.ack_num = safe_increment(self.ack_num, fragment.len() as u32);

                                buf.fill(0);
                            }
                            Err(_) => {}
                        }

                        // Send data if there is enough space in sliding window
                        while !self.data.is_empty() && (self.cur_wnd - self.cur_buf) as usize  > self.data[0].len() {
                            let packet_data = self.data.pop_front().unwrap();
                            let header = TcpHeader {
                                source_port: self.local_port,
                                destination_port: self.remote_port,
                                sequence_number: self.seq_num,
                                ack_number: self.ack_num,
                                header_length: 4,
                                flags: 0b0000_1000,
                                window_size: self.wnd_size
                            };

                            self.register_packet(header, &packet_data);
                            self.cur_buf += packet_data.len() as u16;
                        }
                    }
                    self.status = Status::Finished;
                }
                Status::Finished => {
                    break;
                }
            }
        }
        Ok(())
    }

    fn find_packet_index(in_flight: &VecDeque<Packet>, ack_num: u32) -> Result<usize, ()> {
        for (ind, packet) in in_flight.iter().enumerate() {
            if packet.confirm_ack == ack_num {
                return Ok(ind);
            }
        }
        Err(())
    }

    fn register_packet(&mut self, header: TcpHeader, data: &str) {
        let mut packet_data: Vec<u8> = Vec::new();
        let seq_num = header.sequence_number;
        let ack_num = header.ack_number;

        for d in header.as_bytes() {
            packet_data.push(d);
        }

        let mut data_len: u16 = 0;
        for d in data.as_bytes() {
            packet_data.push(*d);
            data_len += 1;
        }

        if data_len == 0 {
            data_len = 1;
        }

        let packet = Packet {
            timestamp: Instant::now(),
            data: packet_data.clone(),
            seq_num,
            ack_num,
            confirm_ack: safe_increment(seq_num,data_len as u32),
            data_len
        };

        self.in_flight.push_back(packet);

        Self::send_data(&self.remote_host, &self.remote_port, packet_data.as_slice(), &self.socket);

        self.seq_num = safe_increment(seq_num, data_len as u32);
    }

    fn check_retransmission(&mut self) {
        for packet in self.in_flight.iter_mut() {
            let instant = Instant::now();
            let duration = instant.duration_since(packet.timestamp.clone());

            if duration >= Duration::from_millis(self.rto) {
                Self::send_data(&self.remote_host, &self.remote_port, packet.data.as_slice(), &self.socket);
                packet.timestamp = Instant::now();
            } else {
                return;
            }
        }
    }

    fn send_data(remote_host: &str, remote_port: &u16 , packet_data: &[u8], socket: &UdpSocket) {

        loop {
            match socket.send_to(packet_data, format!("{}:{}", remote_host, remote_port)) {
                Ok(_) => {break;}
                Err(e) => {eprintln!("{} -> Failed to send packet at registration", e)}
            }
        }
    }
}