use std::{
    mem,
    collections::VecDeque,
};

#[derive(PartialEq, Default, Debug)]
struct Blob {
    offset: usize,
    data: Vec<u8>
}

#[derive(Debug)]
pub struct Stream {
    preamble: Vec<u8>,
    frames: VecDeque<Blob>,
    offset: usize,
    partial_frame: Vec<u8>,
    decoder: gif::StreamingDecoder,
    undecoded: Vec<u8>,
    decoded: Vec<u8>,
}

impl Stream {
    pub fn new() -> Stream {
        Stream {
            preamble: Vec::new(),
            frames: VecDeque::new(),
            offset: 0,
            partial_frame: Vec::new(),
            decoder: gif::StreamingDecoder::new(),
            undecoded: Vec::new(),
            decoded: Vec::new(),
        }
    }

    pub fn read_after(&self, offset: usize) -> (usize, &[u8]) {
        if offset >= self.offset {
            // we got nothing new
            return (offset, &[]);
        }

        tracing::trace!(?offset, ?self.offset, "read_after");
        if offset < self.preamble.len() {
            return (self.preamble.len(), &self.preamble[offset..]);
        }

        for frame in self.frames.iter().rev() {
            if offset < frame.offset {
                tracing::trace!(%offset, %frame.offset, "frame too late");
                continue;
            }

            let end = frame.offset + frame.data.len();
            if offset >= end {
                // we got nothing new
                tracing::trace!(%offset, %frame.offset, %end, "frame too early");
                return (offset, &[]);
            }

            tracing::trace!(%offset, %frame.offset, %end, "good frame");
            return (end, &frame.data[offset - frame.offset..]);
        }

        // didn't find any frame with offset >= frame.offset,
        // just ship earlieset available frame
        if let Some(frame) = self.frames.front() {
            let end = frame.offset + frame.data.len();
            return (end, &frame.data[..]);
        }

        // we don't have any frames i guess?
        tracing::trace!(%offset, "no frame");
        return (offset, &[]);
    }

    pub fn write(&mut self, mut data: &[u8]) -> eyre::Result<()> {
        //tracing::info!("write");
        if self.undecoded.len() == 0 {
            loop {
                let n = self.consume(data)?;
                if n == 0 {
                    break;
                }
                data = &data[n..];
            }
            let len = data.len();
            if len > 0 {
                tracing::info!(%len, "undecoded leftovers");
            }
            self.undecoded.extend_from_slice(data);
        } else {
            let mut undecoded = mem::replace(&mut self.undecoded, Vec::new());
            undecoded.extend_from_slice(data);
            let data = &undecoded[..];
            loop {
                let n = self.consume(data)?;
                if n == 0 {
                    break;
                }
            }
            let len = data.len();
            if len > 0 {
                tracing::info!(%len, "undecoded leftovers");
            }
            self.undecoded.extend_from_slice(data);
        }
        Ok(())
    }

    fn consume(&mut self, mut data: &[u8]) -> eyre::Result<usize> {
        let mut consumed = 0;
        loop {
            let (n, decoded) = self.decoder.update(data)?;
            consumed += n;
            use gif::Decoded::*;
            match decoded {
                Nothing => {
                    self.decoded.extend_from_slice(&data[..n]);
                    break;
                }
                BlockStart(..)
                    | SubBlockFinished(..)
                    | BlockFinished(..)
                    | Frame(..)
                    | Data(..) => {
                    self.add_to_frame(&data[..n]);
                },
                DataEnd => {
                    self.add_to_frame(&data[..n]);
                    self.finish_frame();
                },
                _ => {
                    self.add_to_preamble(&data[..n]);
                }
            }
            data = &data[n..];
        }

        Ok(consumed)
    }

    fn add_to_preamble(&mut self, data: &[u8]) {
        if self.frames.len() > 0 || self.partial_frame.len() > 0 {
            // we don't want to track non-image-data bits after we've
            // started tracking image data. TODO: this is sketchy
            tracing::warn!("late preamble data");
            return;
        }
        self.partial_frame.append(&mut self.decoded);
        self.preamble.extend_from_slice(data);
        self.offset += data.len();
    }

    fn add_to_frame(&mut self, data: &[u8]) {
        self.partial_frame.append(&mut self.decoded);
        self.partial_frame.extend_from_slice(data);
    }

    fn finish_frame(&mut self) {
        let data = mem::replace(&mut self.partial_frame, Vec::new());
        let offset = self.offset;
        self.offset += data.len();
        self.frames.push_back(Blob { offset, data, });
        tracing::debug!(%self.offset, "finished frame");

        loop {
            let last = self.frames.back().unwrap();
            let first = self.frames.front().unwrap();
            if last.offset - first.offset < 1024*1024*1 {
                break;
            }
            let old_len = self.frames.len();
            self.frames.pop_front();
            let new_len = self.frames.len();
            tracing::debug!(%old_len, %new_len, "evicting frame");
        }
    }
}
