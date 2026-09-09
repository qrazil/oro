//! Stream mechanics that Oro-level tests cannot see: the read buffer, short
//! reads, and the delimiter scan across a refill boundary.

use super::*;

fn scratch(name: &str, contents: &[u8]) -> String {
    let path = std::env::temp_dir().join(format!("oro_stream_test_{name}"));
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn read_returns_at_most_n_and_empty_at_eof() {
    let path = scratch("read_n", b"abcdef");
    let f = OroStream::open_read(&path).unwrap();
    assert_eq!(f.read(4).unwrap(), b"abcd");
    assert_eq!(f.read(4).unwrap(), b"ef");
    assert_eq!(f.read(4).unwrap(), b"");
    // EOF is an empty return, not an exception, and it stays empty.
    assert_eq!(f.read(4).unwrap(), b"");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_zero_is_an_error_not_a_fake_eof() {
    let b = OroStream::buffer(b"xy".to_vec());
    assert!(b.read(0).is_err());
    assert!(b.read(-1).is_err());
}

#[test]
fn the_read_buffer_is_allocated_on_first_read_not_at_construction() {
    // The operational rule from §2: 10,000 idle connections must not be 80 MB
    // of buffers holding nothing.
    let path = scratch("lazy", b"hello");
    let f = OroStream::open_read(&path).unwrap();
    assert_eq!(f.inner.borrow().buf.capacity(), 0, "buffer allocated before any read");
    f.read(1).unwrap();
    assert!(f.inner.borrow().buf.capacity() >= BUFSIZE, "buffer not allocated on first read");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_read_larger_than_the_buffer_bypasses_it() {
    let big = vec![b'z'; BUFSIZE * 2 + 17];
    let path = scratch("big", &big);
    let f = OroStream::open_read(&path).unwrap();
    let got = f.read((BUFSIZE * 2 + 17) as i64).unwrap();
    // A single `read` may legally come up short; what it must not do is copy
    // through the 8 KiB buffer, so the result is one syscall's worth.
    assert!(!got.is_empty() && got.len() <= big.len());
    assert_eq!(f.inner.borrow().buf.capacity(), 0);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_until_spans_a_refill_and_keeps_the_tail() {
    // A delimiter straddling two buffer fills is the case a naive scan gets
    // wrong, so the file is sized to put `\r\n\r\n` across the boundary.
    let mut contents = vec![b'h'; BUFSIZE - 2];
    contents.extend_from_slice(b"\r\n\r\nBODY");
    let path = scratch("until_span", &contents);
    let f = OroStream::open_read(&path).unwrap();
    let head = f.read_until(b"\r\n\r\n", 65536).unwrap();
    assert_eq!(head.len(), BUFSIZE + 2);
    assert!(head.ends_with(b"\r\n\r\n"));
    // Everything after the delimiter is still there for the next read.
    assert_eq!(f.read(8).unwrap(), b"BODY");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_until_raises_past_the_limit_and_stops_at_eof() {
    let b = OroStream::buffer(b"aaaaaaaaaa".to_vec());
    assert!(b.read_until(b"\n", 4).is_err());
    // No delimiter and no more input: return what there is rather than raise,
    // the same rule `read` follows at EOF.
    let c = OroStream::buffer(b"abc".to_vec());
    assert_eq!(c.read_until(b"\n", 64).unwrap(), b"abc");
    let d = OroStream::buffer(b"ab\ncd".to_vec());
    assert_eq!(d.read_until(b"\n", 3).unwrap(), b"ab\n");
}

#[test]
fn a_buffer_is_a_queue_of_bytes() {
    let b = OroStream::buffer(b"one".to_vec());
    b.write(b"two").unwrap();
    assert_eq!(b.read(3).unwrap(), b"one");
    // `bytes()` is what has been written and not yet read.
    assert_eq!(b.bytes().unwrap(), b"two");
    assert_eq!(b.read(3).unwrap(), b"two");
    assert_eq!(b.bytes().unwrap(), b"");
    assert_eq!(b.read(3).unwrap(), b"");
}

#[test]
fn a_buffer_used_as_a_pipe_does_not_grow_without_bound() {
    let b = OroStream::buffer(Vec::new());
    for _ in 0..1000 {
        b.write(&[b'x'; 64]).unwrap();
        assert_eq!(b.read(64).unwrap().len(), 64);
    }
    assert!(b.inner.borrow().buf.len() <= BUFSIZE + 64);
}

#[test]
fn reading_a_writer_and_writing_a_reader_are_errors() {
    let path = scratch("modes", b"data");
    let w = OroStream::open_write(&path).unwrap();
    assert!(w.read(1).is_err());
    let r = OroStream::open_read(&path).unwrap();
    assert!(r.write(b"x").is_err());
    // A closed stream refuses everything but another close.
    r.close().unwrap();
    assert!(r.read(1).is_err());
    assert!(r.close().is_ok());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_all_pre_sizes_from_stat_and_resumes_mid_stream() {
    let contents: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    let path = scratch("all", &contents);
    let f = OroStream::open_read(&path).unwrap();
    let head = f.read(10).unwrap();
    let rest = f.read_all().unwrap();
    assert_eq!(head.len() + rest.len(), contents.len());
    assert_eq!(rest, contents[10..]);
    // One allocation, sized from the file: no chunk-by-chunk doubling, so no
    // 1.5–2× transient peak.
    assert_eq!(rest.capacity(), rest.len());
    std::fs::remove_file(&path).unwrap();
}
