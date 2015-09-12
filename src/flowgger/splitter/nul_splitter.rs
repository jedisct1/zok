use flowgger::decoder::Decoder;
use flowgger::encoder::Encoder;
use std::io::{stderr, Read, Write, BufRead, BufReader};
use std::str;
use std::sync::mpsc::SyncSender;
use super::Splitter;

pub struct NulSplitter;

impl<T: Read> Splitter<T> for NulSplitter {
    fn run(&self, buf_reader: BufReader<T>, tx: SyncSender<Vec<u8>>, decoder: Box<Decoder>, encoder: Box<Encoder>) {
        for line in buf_reader.split(0) {
            let line = match line {
                Err(_) => {
                    let _ = writeln!(stderr(), "EOF?");
                    continue;
                }
                Ok(line) => line
            };
            let line = match str::from_utf8(&line) {
                Err(_) => {
                    let _ = writeln!(stderr(), "Invalid UTF-8 input");
                    continue;
                }
                Ok(line) => line
            };
            if let Err(e) = handle_line(line, &tx, &decoder, &encoder) {
                let _ = writeln!(stderr(), "{}: [{}]", e, line.trim());
            }
        }
    }
}

fn handle_line(line: &str, tx: &SyncSender<Vec<u8>>, decoder: &Box<Decoder>, encoder: &Box<Encoder>) -> Result<(), &'static str> {
    let decoded = try!(decoder.decode(&line));
    let reencoded = try!(encoder.encode(decoded));
    tx.send(reencoded).unwrap();
    Ok(())
}
