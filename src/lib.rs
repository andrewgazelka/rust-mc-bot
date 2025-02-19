use crate::packet_utils::Buf;
use crate::states::{login, play};
use libdeflater::{CompressionLvl, Compressor, Decompressor};
use mio::net::{TcpStream, UnixStream};
use mio::{event, Events, Interest, Poll, Registry, Token};
use rand::prelude::SliceRandom;
use rand::Rng;
use std::collections::HashMap;
use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use antithesis_sdk::random::AntithesisRng;

mod net;
mod packet_processors;
mod packet_utils;
mod states;

const SHOULD_MOVE: bool = true;

const PROTOCOL_VERSION: u32 = 763;

const MESSAGES: &[&str] = &["This is a chat message!", "Wow", "Server = on?"];

pub struct BotManager {
    bot_on: Arc<AtomicU32>,
    addrs: Address,
    bots_per_tick: u32,
    tick_counter: u32,
    action_tick: u32,
    map: HashMap<Token, Bot>,
    packet_buf: Buf,
    uncompressed_buf: Buf,
    compression: Compression,
    poll: Poll,
    events: Events,
    dur: Duration,
    count: u32,
}

impl BotManager {
    pub fn create(count: u32, addrs: Address, bot_on: Arc<AtomicU32>) -> anyhow::Result<Self> {
        antithesis_sdk::antithesis_init();

        let poll = Poll::new().expect("could not unwrap poll");
        //todo check used cap
        let events = Events::with_capacity((count * 5) as usize);
        let map = HashMap::new();

        let bots_per_tick = 1;

        let packet_buf = Buf::with_length(2000);
        let uncompressed_buf = Buf::with_length(2000);

        let compression = Compression {
            compressor: Compressor::new(CompressionLvl::default()),
            decompressor: Decompressor::new(),
        };

        let dur = Duration::from_millis(50);

        let tick_counter = 0;
        let action_tick = 4;

        Ok(BotManager {
            bot_on,
            addrs,
            bots_per_tick,
            tick_counter,
            action_tick,
            map,
            packet_buf,
            uncompressed_buf,
            compression,
            poll,
            events,
            dur,
            count,
        })
    }

    pub fn game_loop(&mut self) {
        loop {
            let start = Instant::now();
            self.tick();

            let elapsed = start.elapsed();

            if elapsed > self.dur {
                continue;
            }

            if self.map.is_empty() {
                break;
            }

            std::thread::sleep(self.dur - elapsed);
        }
    }

    pub fn tick(&mut self) {
        let bots_joined = self.bot_on.fetch_add(self.bots_per_tick, Ordering::Relaxed);
        if bots_joined < self.count {
            let registry = self.poll.registry();
            for bot_id in bots_joined..(self.bots_per_tick + bots_joined.min(self.count)) {
                let token = Token(bot_id as usize);

                let name = format!("Bot_{bot_id}");

                let mut bot = Bot {
                    token,
                    stream: self.addrs.connect(),
                    name,
                    id: bot_id,
                    entity_id: 0,
                    compression_threshold: 0,
                    state: 0,
                    kicked: false,
                    teleported: false,
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    buffering_buf: Buf::with_length(200),
                    joined: false,
                };
                registry
                    .register(
                        &mut bot.stream,
                        bot.token,
                        Interest::READABLE | Interest::WRITABLE,
                    )
                    .expect("could not register");

                self.map.insert(token, bot);
            }
        } else {
            // go down again
            self.bot_on.fetch_sub(self.bots_per_tick, Ordering::Relaxed);
        }

        fn start_bot(bot: &mut Bot, compression: &mut Compression) {
            bot.joined = true;

            // socket ops
            bot.stream.set_ops();

            //login sequence
            let buf = login::write_handshake_packet(PROTOCOL_VERSION, "".to_string(), 0, 2);
            bot.send_packet(buf, compression);

            let buf = login::write_login_start_packet(&bot.name);
            bot.send_packet(buf, compression);

            // println!("bot \"{}\" joined", bot.name);
        }

        self.poll
            .poll(&mut self.events, None)
            .expect("couldn't poll");
        for event in self.events.iter() {
            if let Some(bot) = self.map.get_mut(&event.token()) {
                if event.is_writable() && !bot.joined {
                    start_bot(bot, &mut self.compression);
                }
                if event.is_readable() && bot.joined {
                    net::process_packet(
                        bot,
                        &mut self.packet_buf,
                        &mut self.uncompressed_buf,
                        &mut self.compression,
                    );
                    if bot.kicked {
                        println!("{} disconnected", bot.name);
                        let token = bot.token;
                        self.map.remove(&token).expect("kicked bot doesn't exist");

                        if self.map.is_empty() {
                            return;
                        }
                    }
                }
            }
        }

        let mut to_remove = Vec::new();

        for bot in self.map.values_mut() {
            if SHOULD_MOVE && bot.teleported {
                bot.x += rand::random::<f64>() * 1.0 - 0.5;
                bot.z += rand::random::<f64>() * 1.0 - 0.5;
                bot.send_packet(play::write_current_pos(bot), &mut self.compression);

                if (self.tick_counter + bot.id) % self.action_tick == 0 {
                    match AntithesisRng.gen_range(0..=4u8) {
                        0 => {
                            // Send chat
                            bot.send_packet(
                                play::write_chat_message(
                                    MESSAGES.choose(&mut AntithesisRng).unwrap(),
                                ),
                                &mut self.compression,
                            );
                        }
                        1 => {
                            // Punch animation
                            bot.send_packet(
                                play::write_animation(rand::random()),
                                &mut self.compression,
                            );
                        }
                        2 => {
                            // Sneak
                            bot.send_packet(
                                play::write_entity_action(
                                    bot.entity_id,
                                    if rand::random() { 1 } else { 0 },
                                    0,
                                ),
                                &mut self.compression,
                            );
                        }
                        3 => {
                            // Sprint
                            bot.send_packet(
                                play::write_entity_action(
                                    bot.entity_id,
                                    if rand::random() { 3 } else { 4 },
                                    0,
                                ),
                                &mut self.compression,
                            );
                        }
                        4 => {
                            // Held item
                            bot.send_packet(
                                play::write_held_slot(AntithesisRng.gen_range(0..9)),
                                &mut self.compression,
                            );
                        }
                        _ => {}
                    }
                }
            }

            if bot.kicked {
                to_remove.push(bot.token);
            }
        }

        for bot in to_remove {
            let _ = self.map.remove(&bot);
        }

        self.tick_counter += 1;
    }
}

#[derive(Clone, Debug)]
pub enum Address {
    #[cfg(unix)]
    UNIX(PathBuf),
    TCP(SocketAddr),
}

impl Address {
    pub fn connect(&self) -> Stream {
        match self {
            #[cfg(unix)]
            Address::UNIX(path) => {
                Stream::UNIX(UnixStream::connect(path).expect("Could not connect to the server"))
            }
            Address::TCP(address) => Stream::TCP(
                TcpStream::connect(address.to_owned()).expect("Could not connect to the server"),
            ),
        }
    }
}

pub enum Stream {
    #[cfg(unix)]
    UNIX(UnixStream),
    TCP(TcpStream),
}

impl Stream {
    pub fn set_ops(&mut self) {
        // match self {
        //     Stream::TCP(s) => {
        //         s.set_nodelay(true).unwrap();
        //     }
        //     _ => {}
        // }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.read(buf),
            Stream::TCP(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.write(buf),
            Stream::TCP(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.flush(),
            Stream::TCP(s) => s.flush(),
        }
    }
}

impl event::Source for Stream {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.register(registry, token, interests),
            Stream::TCP(s) => s.register(registry, token, interests),
        }
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.reregister(registry, token, interests),
            Stream::TCP(s) => s.reregister(registry, token, interests),
        }
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::UNIX(s) => s.deregister(registry),
            Stream::TCP(s) => s.deregister(registry),
        }
    }
}

pub struct Compression {
    compressor: Compressor,
    decompressor: Decompressor,
}

pub struct Bot {
    pub token: Token,
    pub stream: Stream,
    pub name: String,
    pub id: u32,
    pub entity_id: u32,
    pub compression_threshold: i32,
    pub state: u8,
    pub kicked: bool,
    pub teleported: bool,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub buffering_buf: Buf,
    pub joined: bool,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
