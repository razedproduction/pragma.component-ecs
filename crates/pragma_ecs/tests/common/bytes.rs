//! A reader of fuzzer input that never fails: once the input ends, it yields zeros.

/// Reads little-endian values from the front of a byte slice.
#[derive(Debug)]
pub struct Bytes<'a> {
    data: &'a [u8],
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn byte(&mut self) -> u8 {
        match self.data.split_first() {
            Some((first, rest)) => {
                self.data = rest;
                *first
            }
            None => 0,
        }
    }

    pub fn bool(&mut self) -> bool {
        self.byte() & 1 == 1
    }

    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.byte(), self.byte(), self.byte(), self.byte()])
    }

    pub fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.u32().to_le_bytes())
    }

    /// A raw index, reduced modulo the length of whatever it picks from.
    pub fn index(&mut self) -> usize {
        usize::from(self.u16())
    }

    /// A value in `0..bound` (`bound > 0`).
    pub fn below(&mut self, bound: u8) -> u8 {
        self.byte() % bound
    }
}

/// Picks `raw % len` from `items`, or `None` if it is empty.
pub fn pick<T: Copy>(items: &[T], raw: usize) -> Option<T> {
    if items.is_empty() {
        None
    } else {
        items.get(raw % items.len()).copied()
    }
}
