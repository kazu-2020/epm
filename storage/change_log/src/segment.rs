use bytes::{Buf, BufMut, Bytes, BytesMut};

const MAGIC: [u8; 4] = *b"CLOG";
const VERSION_WITHOUT_PROCESS: u8 = 1;
const VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessId(pub u128);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(pub u64);

impl Sequence {
    pub fn next(self) -> Sequence {
        Sequence(self.0 + 1)
    }

    pub fn index(self) -> usize {
        usize::try_from(self.0).expect("a Commit Sequence fits in usize")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub Bytes);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub request_id: RequestId,
    pub sequence: Sequence,
    pub payload: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub epoch: Epoch,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Fence(ProcessId),
    Commits(Vec<Commit>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Magic,
    Version(u8),
    Truncated,
    TrailingBytes,
    ContentKind(u8),
}

impl Segment {
    pub fn commits(&self) -> &[Commit] {
        match &self.content {
            Content::Fence(_) => &[],
            Content::Commits(commits) => commits,
        }
    }

    pub fn decode(bytes: Bytes) -> Result<Segment, DecodeError> {
        let mut reader = Reader { rest: bytes };
        if reader.take(MAGIC.len())? != MAGIC[..] {
            return Err(DecodeError::Magic);
        }
        let version = reader.u8()?;
        let epoch = match version {
            VERSION | VERSION_WITHOUT_PROCESS => Epoch(reader.u64()?),
            other => return Err(DecodeError::Version(other)),
        };
        let content = match version {
            VERSION_WITHOUT_PROCESS => Content::Commits(reader.commits()?),
            _ => match reader.u8()? {
                0 => Content::Commits(reader.commits()?),
                1 => Content::Fence(ProcessId(reader.u128()?)),
                kind => return Err(DecodeError::ContentKind(kind)),
            },
        };
        if reader.rest.has_remaining() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Segment { epoch, content })
    }
}

pub fn encode(segment: &Segment) -> Bytes {
    let mut out = BytesMut::new();
    out.put_slice(&MAGIC);
    out.put_u8(VERSION);
    out.put_u64_le(segment.epoch.0);
    match &segment.content {
        Content::Commits(commits) => {
            out.put_u8(0);
            out.put_u32_le(length(commits.len()));
            for commit in commits {
                put_prefixed(&mut out, &commit.request_id.0);
                out.put_u64_le(commit.sequence.0);
                put_prefixed(&mut out, &commit.payload);
            }
        }
        Content::Fence(process) => {
            out.put_u8(1);
            out.put_u128_le(process.0);
        }
    }
    out.freeze()
}

fn put_prefixed(out: &mut BytesMut, bytes: &[u8]) {
    out.put_u32_le(length(bytes.len()));
    out.put_slice(bytes);
}

fn length(length: usize) -> u32 {
    u32::try_from(length).expect("a segment holds less than 4 GiB per field and 2^32 commits")
}

struct Reader {
    rest: Bytes,
}

impl Reader {
    fn take(&mut self, length: usize) -> Result<Bytes, DecodeError> {
        if self.rest.remaining() < length {
            return Err(DecodeError::Truncated);
        }
        Ok(self.rest.split_to(length))
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?.get_u8())
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(self.take(4)?.get_u32_le())
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(self.take(8)?.get_u64_le())
    }

    fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(self.take(16)?.get_u128_le())
    }

    fn prefixed(&mut self) -> Result<Bytes, DecodeError> {
        let length = self.u32()?;
        self.take(length as usize)
    }

    fn commits(&mut self) -> Result<Vec<Commit>, DecodeError> {
        let count = self.u32()?;
        // Not Vec::with_capacity(count): the count is untrusted until the bytes behind it are read.
        let mut commits = Vec::new();
        for _ in 0..count {
            // Not a slice of the segment: the assignee keeps every Request ID, and a slice would keep the whole segment with its payloads.
            let request_id = RequestId(Bytes::copy_from_slice(&self.prefixed()?));
            let sequence = Sequence(self.u64()?);
            let payload = self.prefixed()?;
            commits.push(Commit {
                request_id,
                sequence,
                payload,
            });
        }
        Ok(commits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(request_id: &'static str, sequence: u64, payload: &'static str) -> Commit {
        Commit {
            request_id: RequestId(Bytes::from_static(request_id.as_bytes())),
            sequence: Sequence(sequence),
            payload: Bytes::from_static(payload.as_bytes()),
        }
    }

    fn sample() -> Segment {
        Segment {
            epoch: Epoch(7),
            content: Content::Commits(vec![commit("r1", 4, "edit a"), commit("r2", 5, "")]),
        }
    }

    fn fence() -> Segment {
        Segment {
            epoch: Epoch(2),
            content: Content::Fence(ProcessId(0x0102)),
        }
    }

    #[test]
    fn a_segment_reads_back_as_written() {
        for segment in [sample(), fence()] {
            assert_eq!(Segment::decode(encode(&segment)), Ok(segment));
        }
    }

    #[test]
    fn the_layout_is_fixed() {
        assert_eq!(
            &encode(&fence())[..],
            b"CLOG\x02\x02\0\0\0\0\0\0\0\x01\x02\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0"
        );
        let one = Segment {
            epoch: Epoch(1),
            content: Content::Commits(vec![commit("r", 3, "p")]),
        };
        assert_eq!(
            &encode(&one)[..],
            b"CLOG\x02\x01\0\0\0\0\0\0\0\0\x01\0\0\0\x01\0\0\0r\x03\0\0\0\0\0\0\0\x01\0\0\0p"
        );
    }

    #[test]
    fn a_segment_of_the_first_version_reads_as_commits() {
        assert_eq!(
            Segment::decode(Bytes::from_static(b"CLOG\x01\x02\0\0\0\0\0\0\0\0\0\0\0")),
            Ok(Segment {
                epoch: Epoch(2),
                content: Content::Commits(vec![]),
            })
        );
        assert_eq!(
            Segment::decode(Bytes::from_static(
                b"CLOG\x01\x01\0\0\0\0\0\0\0\x01\0\0\0\x01\0\0\0r\x03\0\0\0\0\0\0\0\x01\0\0\0p"
            )),
            Ok(Segment {
                epoch: Epoch(1),
                content: Content::Commits(vec![commit("r", 3, "p")]),
            })
        );
    }

    #[test]
    fn an_unknown_content_kind_is_an_error() {
        let mut kind = encode(&fence()).to_vec();
        kind[13] = 2;
        assert_eq!(
            Segment::decode(kind.into()),
            Err(DecodeError::ContentKind(2))
        );
    }

    #[test]
    fn a_decoded_request_id_does_not_hold_the_segment() {
        let bytes = encode(&sample());
        let segment = Segment::decode(bytes.clone()).expect("valid");
        let mut request_id = segment.commits()[0].request_id.0.clone();
        drop(segment);
        assert!(request_id.is_unique(), "shares the buffer of the segment");
        assert_eq!(request_id.split_to(2), "r1");
    }

    #[test]
    fn every_truncation_is_an_error() {
        for bytes in [encode(&sample()), encode(&fence())] {
            for end in 0..bytes.len() {
                assert_eq!(
                    Segment::decode(bytes.slice(..end)),
                    Err(DecodeError::Truncated),
                    "cut at {end}"
                );
            }
        }
    }

    #[test]
    fn bad_headers_and_trailing_bytes_are_errors() {
        for bytes in [encode(&sample()), encode(&fence())] {
            let mut trailing = bytes.to_vec();
            trailing.push(0);
            assert_eq!(
                Segment::decode(trailing.into()),
                Err(DecodeError::TrailingBytes)
            );
        }
        let bytes = encode(&sample());
        let mut magic = bytes.to_vec();
        magic[0] = b'X';
        assert_eq!(Segment::decode(magic.into()), Err(DecodeError::Magic));
        let mut version = bytes.to_vec();
        version[4] = 3;
        assert_eq!(
            Segment::decode(version.into()),
            Err(DecodeError::Version(3))
        );
    }

    #[test]
    fn a_length_past_the_end_is_an_error() {
        let mut huge = b"CLOG\x02\x01\0\0\0\0\0\0\0\0\xff\xff\xff\xff".to_vec();
        assert_eq!(
            Segment::decode(huge.clone().into()),
            Err(DecodeError::Truncated)
        );
        huge.truncate(14);
        huge.extend_from_slice(b"\x01\0\0\0\xff\xff\xff\xff");
        assert_eq!(Segment::decode(huge.into()), Err(DecodeError::Truncated));
    }
}
