//! What [`read_request_blocking`] guarantees a one-shot mock: that it answers only once the
//! whole request is in, however the socket split it.

use std::{collections::VecDeque, io::Read};

use super::read_request_blocking;

/// A socket that hands over `reads` one per `read` call, then reports the peer closed.
struct Segments(VecDeque<&'static [u8]>);

impl Read for Segments {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let Some(next) = self.0.pop_front() else {
            return Ok(0);
        };
        out[..next.len()].copy_from_slice(next);
        Ok(next.len())
    }
}

const HEAD: &[u8] = b"POST /jmap/ HTTP/1.1\r\ncontent-length: 4\r\n\r\n";

#[test]
fn a_body_that_arrives_after_the_head_is_read_too() {
    // The shape that reset the connection on CI: the head in one read, the body in the next.
    let mut socket = Segments(VecDeque::from([HEAD, b"bo".as_slice(), b"dy"]));
    assert_eq!(read_request_blocking(&mut socket), [HEAD, b"body"].concat());
}

#[test]
fn a_head_split_across_reads_is_joined() {
    let (start, end) = HEAD.split_at(10);
    let mut socket = Segments(VecDeque::from([start, end, b"body"]));
    assert_eq!(read_request_blocking(&mut socket), [HEAD, b"body"].concat());
}

#[test]
fn a_request_without_a_body_ends_at_the_blank_line() {
    let get: &[u8] = b"GET /session HTTP/1.1\r\nhost: test.local\r\n\r\n";
    // A second read would block on a real socket, so none may be made.
    let mut socket = Segments(VecDeque::from([get, b"never read"]));
    assert_eq!(read_request_blocking(&mut socket), get);
}

#[test]
fn a_client_that_closes_early_hands_back_what_arrived() {
    let mut socket = Segments(VecDeque::from([HEAD, b"bo"]));
    assert_eq!(read_request_blocking(&mut socket), [HEAD, b"bo"].concat());
}
