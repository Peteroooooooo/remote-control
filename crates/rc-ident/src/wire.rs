use crate::Error;

/// Reads fixed fields off a payload; every read checks the length.
pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader(b)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Length);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub fn finish(self) -> Result<(), Error> {
        if self.0.is_empty() { Ok(()) } else { Err(Error::Length) }
    }
}
