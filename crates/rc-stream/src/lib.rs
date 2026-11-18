//! Transport logic for the video stream, kept free of I/O so it can be unit tested:
//! - [`wire`]: datagram layout.
//! - [`fec`]: split a frame into shards plus Reed-Solomon parity, and put it back together.
//! - [`pacer`]: spread a frame's datagrams over part of the frame interval.
//! - [`rate`]: pick the encoder bitrate and FEC share from delivery and loss feedback.
//! - [`budget`]: size (or skip) the next frame while earlier ones are still queued.

pub mod budget;
pub mod fec;
pub mod pacer;
pub mod rate;
pub mod wire;
