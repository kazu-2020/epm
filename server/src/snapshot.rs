use bytes::{Buf, BufMut, Bytes, BytesMut};
use change_log::{RequestId, Slot, Start};
use dimension::{Dimension, DimensionUlid};
use measure::{Measure, MeasureDefinition, MeasureError, MeasureUlid};
use ulid::Ulid;

const MAGIC: [u8; 4] = *b"SNAP";
const VERSION: u8 = 1;
const CELLS_PER_CHUNK: usize = 4 << 20;
const REQUEST_IDS_PER_CHUNK: usize = 1 << 20;
const LEVEL: i32 = 1;

#[derive(Debug)]
pub struct Snapshot {
    pub start: Start,
    pub inputs: Vec<Measure>,
}

#[derive(Debug, PartialEq)]
pub struct Object {
    pub path: String,
    pub bytes: Bytes,
}

#[derive(Debug, PartialEq)]
pub struct Manifest {
    slot: Slot,
    request_chunks: Vec<Chunk>,
    measures: Vec<MeasureEntry>,
}

#[derive(Debug, PartialEq)]
struct MeasureEntry {
    ulid: MeasureUlid,
    axes: Vec<(DimensionUlid, u32)>,
    chunks: Vec<Chunk>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Chunk {
    count: u32,
    length: u32,
}

#[derive(Debug, PartialEq)]
pub enum SnapshotError {
    Magic,
    Version(u8),
    Truncated,
    TrailingBytes,
    ObjectCount { expected: usize, found: usize },
    Corrupt { path: String },
    Definitions(MeasureUlid),
    Measure(MeasureUlid, MeasureError),
}

pub fn encode(snapshot: &Snapshot) -> (Vec<Object>, Bytes) {
    encode_in_chunks(snapshot, REQUEST_IDS_PER_CHUNK, CELLS_PER_CHUNK)
}

fn encode_in_chunks(
    snapshot: &Snapshot,
    request_ids_per_chunk: usize,
    cells_per_chunk: usize,
) -> (Vec<Object>, Bytes) {
    let mut objects = Vec::new();
    let mut request_chunks = Vec::new();
    for (index, request_ids) in snapshot
        .start
        .request_ids
        .chunks(request_ids_per_chunk)
        .enumerate()
    {
        let mut out = BytesMut::new();
        for request_id in request_ids {
            put_varint(&mut out, request_id.0.len() as u64);
            out.put_slice(&request_id.0);
        }
        let (chunk, bytes) = compress(request_ids.len(), &out);
        request_chunks.push(chunk);
        objects.push(Object {
            path: request_path(index),
            bytes,
        });
    }
    let mut measures = Vec::new();
    for input in &snapshot.inputs {
        let compact = input.compact();
        let mut chunks = Vec::new();
        let keys = compact.keys().chunks(cells_per_chunk);
        let values = compact.values().chunks(cells_per_chunk);
        for (index, (keys, values)) in keys.zip(values).enumerate() {
            let mut out = BytesMut::with_capacity(keys.len() * 9);
            let mut previous = 0;
            for (&key, &value) in keys.iter().zip(values) {
                put_varint(&mut out, key - previous);
                out.put_f64_le(value);
                previous = key;
            }
            let (chunk, bytes) = compress(keys.len(), &out);
            chunks.push(chunk);
            objects.push(Object {
                path: measure_path(compact.ulid(), index),
                bytes,
            });
        }
        measures.push(MeasureEntry {
            ulid: compact.ulid(),
            axes: compact.axis_lengths().collect(),
            chunks,
        });
    }
    let manifest = Manifest {
        slot: snapshot.start.slot,
        request_chunks,
        measures,
    };
    (objects, manifest.encode())
}

impl Manifest {
    pub fn decode(bytes: Bytes) -> Result<Manifest, SnapshotError> {
        let mut reader = Reader { rest: bytes };
        if reader.take(MAGIC.len())? != MAGIC[..] {
            return Err(SnapshotError::Magic);
        }
        let version = reader.u8()?;
        if version != VERSION {
            return Err(SnapshotError::Version(version));
        }
        let slot = Slot(reader.u64()?);
        let request_chunks = reader.chunks()?;
        let mut measures = Vec::new();
        for _ in 0..reader.u32()? {
            let ulid = MeasureUlid(reader.ulid()?);
            let mut axes = Vec::new();
            for _ in 0..reader.u32()? {
                axes.push((DimensionUlid(reader.ulid()?), reader.u32()?));
            }
            let chunks = reader.chunks()?;
            measures.push(MeasureEntry { ulid, axes, chunks });
        }
        if reader.rest.has_remaining() {
            return Err(SnapshotError::TrailingBytes);
        }
        Ok(Manifest {
            slot,
            request_chunks,
            measures,
        })
    }

    pub fn paths(&self) -> Vec<String> {
        let requests = (0..self.request_chunks.len()).map(request_path);
        let measures = self.measures.iter().flat_map(|measure| {
            (0..measure.chunks.len()).map(|index| measure_path(measure.ulid, index))
        });
        requests.chain(measures).collect()
    }

    pub fn decode_objects(
        &self,
        objects: Vec<Bytes>,
        definitions: &[MeasureDefinition],
        dimensions: &[&Dimension],
    ) -> Result<Snapshot, SnapshotError> {
        let paths = self.paths();
        if objects.len() != paths.len() {
            return Err(SnapshotError::ObjectCount {
                expected: paths.len(),
                found: objects.len(),
            });
        }
        let mut objects = paths.into_iter().zip(objects);
        let mut request_ids = Vec::new();
        for chunk in &self.request_chunks {
            let (path, bytes) = objects.next().expect("one object per path");
            let mut reader = Reader {
                rest: decompress(*chunk, &bytes, &path)?,
            };
            reader
                .request_ids(chunk.count, &mut request_ids)
                .map_err(|_| SnapshotError::Corrupt { path })?;
        }
        let mut inputs = Vec::new();
        for entry in &self.measures {
            let definition = definitions
                .iter()
                .find(|definition| definition.ulid == entry.ulid)
                .filter(|definition| axes(definition, dimensions).as_ref() == Some(&entry.axes))
                .ok_or(SnapshotError::Definitions(entry.ulid))?;
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for chunk in &entry.chunks {
                let (path, bytes) = objects.next().expect("one object per path");
                let mut reader = Reader {
                    rest: decompress(*chunk, &bytes, &path)?,
                };
                reader
                    .cells(chunk.count, &mut keys, &mut values)
                    .map_err(|_| SnapshotError::Corrupt { path })?;
            }
            let measure =
                Measure::try_from_keys(definition.clone(), keys.into(), values.into(), dimensions)
                    .map_err(|error| SnapshotError::Measure(entry.ulid, error))?;
            inputs.push(measure);
        }
        if let Some(unmatched) = definitions.iter().find(|definition| {
            self.measures
                .iter()
                .filter(|entry| entry.ulid == definition.ulid)
                .count()
                != 1
        }) {
            return Err(SnapshotError::Definitions(unmatched.ulid));
        }
        Ok(Snapshot {
            start: Start {
                slot: self.slot,
                request_ids,
            },
            inputs,
        })
    }

    fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_slice(&MAGIC);
        out.put_u8(VERSION);
        out.put_u64_le(self.slot.0);
        put_chunks(&mut out, &self.request_chunks);
        out.put_u32_le(count(self.measures.len()));
        for measure in &self.measures {
            out.put_u128_le(measure.ulid.0.0);
            out.put_u32_le(count(measure.axes.len()));
            for (axis, length) in &measure.axes {
                out.put_u128_le(axis.0.0);
                out.put_u32_le(*length);
            }
            put_chunks(&mut out, &measure.chunks);
        }
        out.freeze()
    }
}

fn axes(
    definition: &MeasureDefinition,
    dimensions: &[&Dimension],
) -> Option<Vec<(DimensionUlid, u32)>> {
    definition
        .axes
        .iter()
        .map(|&axis| {
            let dimension = dimensions
                .iter()
                .find(|dimension| dimension.ulid() == axis)?;
            Some((axis, dimension.len()))
        })
        .collect()
}

fn request_path(index: usize) -> String {
    format!("requests/{index:06}")
}

fn measure_path(measure: MeasureUlid, index: usize) -> String {
    format!("{}/{index:06}", measure.0)
}

fn compress(cells: usize, uncompressed: &[u8]) -> (Chunk, Bytes) {
    let mut compressor = zstd::bulk::Compressor::new(LEVEL).expect("a valid zstd level");
    compressor
        .include_checksum(true)
        .expect("zstd takes the checksum flag");
    let bytes = compressor.compress(uncompressed).expect("zstd compresses");
    let chunk = Chunk {
        count: count(cells),
        length: count(uncompressed.len()),
    };
    (chunk, bytes.into())
}

fn decompress(chunk: Chunk, bytes: &[u8], path: &str) -> Result<Bytes, SnapshotError> {
    let corrupt = || SnapshotError::Corrupt {
        path: path.to_owned(),
    };
    let uncompressed =
        zstd::bulk::decompress(bytes, chunk.length as usize).map_err(|_| corrupt())?;
    if uncompressed.len() != chunk.length as usize {
        return Err(corrupt());
    }
    Ok(uncompressed.into())
}

fn put_chunks(out: &mut BytesMut, chunks: &[Chunk]) {
    out.put_u32_le(count(chunks.len()));
    for chunk in chunks {
        out.put_u32_le(chunk.count);
        out.put_u32_le(chunk.length);
    }
}

fn put_varint(out: &mut BytesMut, mut value: u64) {
    while value >= 0x80 {
        out.put_u8(value as u8 | 0x80);
        value >>= 7;
    }
    out.put_u8(value as u8);
}

fn count(length: usize) -> u32 {
    u32::try_from(length).expect("a chunk and a manifest list hold less than 2^32 items")
}

struct Reader {
    rest: Bytes,
}

impl Reader {
    fn take(&mut self, length: usize) -> Result<Bytes, SnapshotError> {
        if self.rest.remaining() < length {
            return Err(SnapshotError::Truncated);
        }
        Ok(self.rest.split_to(length))
    }

    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?.get_u8())
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(self.take(4)?.get_u32_le())
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(self.take(8)?.get_u64_le())
    }

    fn f64(&mut self) -> Result<f64, SnapshotError> {
        Ok(self.take(8)?.get_f64_le())
    }

    fn ulid(&mut self) -> Result<Ulid, SnapshotError> {
        Ok(Ulid(self.take(16)?.get_u128_le()))
    }

    fn varint(&mut self) -> Result<u64, SnapshotError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err(SnapshotError::Truncated);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err(SnapshotError::Truncated)
    }

    fn request_ids(
        &mut self,
        count: u32,
        request_ids: &mut Vec<RequestId>,
    ) -> Result<(), SnapshotError> {
        for _ in 0..count {
            let length = self.varint()?;
            let length = usize::try_from(length).map_err(|_| SnapshotError::Truncated)?;
            request_ids.push(RequestId(self.take(length)?));
        }
        self.end()
    }

    fn cells(
        &mut self,
        count: u32,
        keys: &mut Vec<u64>,
        values: &mut Vec<f64>,
    ) -> Result<(), SnapshotError> {
        let mut previous = 0u64;
        for _ in 0..count {
            previous = previous
                .checked_add(self.varint()?)
                .ok_or(SnapshotError::Truncated)?;
            keys.push(previous);
            values.push(self.f64()?);
        }
        self.end()
    }

    fn chunks(&mut self) -> Result<Vec<Chunk>, SnapshotError> {
        let mut chunks = Vec::new();
        for _ in 0..self.u32()? {
            chunks.push(Chunk {
                count: self.u32()?,
                length: self.u32()?,
            });
        }
        Ok(chunks)
    }

    fn end(&self) -> Result<(), SnapshotError> {
        if self.rest.has_remaining() {
            return Err(SnapshotError::TrailingBytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use datagen::representative::{self, Size};
    use workspace::Workspace;

    use super::*;

    fn workspace() -> Workspace {
        representative::generate(&Size::TINY, 7)
    }

    fn snapshot(workspace: &Workspace) -> Snapshot {
        Snapshot {
            start: Start {
                slot: Slot(3),
                request_ids: ["r1", "", "r3"]
                    .map(|name| RequestId(Bytes::from_static(name.as_bytes())))
                    .to_vec(),
            },
            inputs: workspace.inputs.iter().map(Measure::compact).collect(),
        }
    }

    fn definitions(workspace: &Workspace) -> Vec<MeasureDefinition> {
        workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .collect()
    }

    fn read_back(
        manifest: Bytes,
        objects: Vec<Object>,
        definitions: &[MeasureDefinition],
        dimensions: &[&Dimension],
    ) -> Result<Snapshot, SnapshotError> {
        let manifest = Manifest::decode(manifest)?;
        let paths: Vec<_> = objects.iter().map(|object| object.path.clone()).collect();
        assert_eq!(manifest.paths(), paths);
        let bytes = objects.into_iter().map(|object| object.bytes).collect();
        manifest.decode_objects(bytes, definitions, dimensions)
    }

    fn cells(measures: &[Measure]) -> Vec<(MeasureDefinition, Vec<u64>, Vec<u64>)> {
        measures
            .iter()
            .map(|measure| {
                (
                    measure.definition().clone(),
                    measure.keys().to_vec(),
                    measure
                        .values()
                        .iter()
                        .map(|value| value.to_bits())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_snapshot_reads_back_as_written_across_chunks() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let snapshot = snapshot(&workspace);
        let (objects, manifest) = encode_in_chunks(&snapshot, 2, 5);
        assert!(
            objects.len() > 2 + workspace.inputs.len(),
            "more than one chunk per measure"
        );
        let read = read_back(manifest, objects, &definitions(&workspace), &dimensions)
            .expect("reads back");
        assert_eq!(read.start, snapshot.start);
        assert_eq!(cells(&read.inputs), cells(&snapshot.inputs));
    }

    #[test]
    fn the_same_snapshot_gives_the_same_bytes() {
        let workspace = workspace();
        let snapshot = snapshot(&workspace);
        assert_eq!(encode(&snapshot), encode(&snapshot));
    }

    #[test]
    fn the_manifest_layout_is_fixed() {
        let empty = Snapshot {
            start: Start {
                slot: Slot(2),
                request_ids: vec![],
            },
            inputs: vec![],
        };
        let (objects, manifest) = encode(&empty);
        assert!(objects.is_empty());
        assert_eq!(&manifest[..], b"SNAP\x01\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    }

    #[test]
    fn a_snapshot_that_does_not_match_the_definitions_is_an_error() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let (objects, manifest) = encode(&snapshot(&workspace));
        let mut fewer = definitions(&workspace);
        let missing = fewer.pop().expect("an input").ulid;
        let first = fewer[0].ulid;
        assert_eq!(
            read_back(manifest.clone(), objects, &fewer, &dimensions).err(),
            Some(SnapshotError::Definitions(missing)),
            "a measure without a definition"
        );
        let (objects, _) = encode(&snapshot(&workspace));
        let mut moved = definitions(&workspace);
        moved[0].axes.reverse();
        assert_eq!(
            read_back(manifest, objects, &moved, &dimensions).err(),
            Some(SnapshotError::Definitions(first)),
            "a definition with other axes"
        );
    }

    #[test]
    fn a_changed_chunk_is_corrupt() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let (mut objects, manifest) = encode(&snapshot(&workspace));
        let last = objects.last_mut().expect("an object");
        let mut bytes = last.bytes.to_vec();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        last.bytes = bytes.into();
        let path = last.path.clone();
        assert_eq!(
            read_back(manifest, objects, &definitions(&workspace), &dimensions).err(),
            Some(SnapshotError::Corrupt { path })
        );
    }

    #[test]
    fn every_truncation_of_the_manifest_is_an_error() {
        let workspace = workspace();
        let (_, manifest) = encode(&snapshot(&workspace));
        for end in 0..manifest.len() {
            assert_eq!(
                Manifest::decode(manifest.slice(..end)),
                Err(SnapshotError::Truncated),
                "cut at {end}"
            );
        }
    }

    #[test]
    fn the_manifest_names_its_objects_and_the_lengths_of_their_axes() {
        let workspace = workspace();
        let (objects, manifest) = encode(&snapshot(&workspace));
        let manifest = Manifest::decode(manifest).expect("valid");
        let mut expected = vec!["requests/000000".to_owned()];
        for input in &workspace.inputs {
            expected.push(format!("{}/000000", input.ulid().0));
        }
        assert_eq!(manifest.paths(), expected);
        assert_eq!(objects.len(), expected.len());
        for (entry, input) in manifest.measures.iter().zip(&workspace.inputs) {
            let lengths: Vec<_> = input
                .definition()
                .axes
                .iter()
                .map(|&axis| {
                    let dimension = workspace
                        .dimensions
                        .iter()
                        .find(|dimension| dimension.ulid() == axis)
                        .expect("a dimension");
                    (axis, dimension.len())
                })
                .collect();
            assert_eq!(entry.axes, lengths);
        }
    }

    #[test]
    fn a_definition_without_a_measure_in_the_snapshot_is_an_error() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let mut fewer = snapshot(&workspace);
        let dropped = fewer.inputs.pop().expect("an input").ulid();
        let (objects, manifest) = encode(&fewer);
        assert_eq!(
            read_back(manifest, objects, &definitions(&workspace), &dimensions).err(),
            Some(SnapshotError::Definitions(dropped))
        );
    }

    #[test]
    fn a_chunk_with_bytes_after_its_cells_is_an_error() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let (mut objects, manifest) = encode(&snapshot(&workspace));
        let mut manifest = Manifest::decode(manifest).expect("valid");
        let mut uncompressed = BytesMut::new();
        for name in ["r1", "", "r3", "extra"] {
            put_varint(&mut uncompressed, name.len() as u64);
            uncompressed.put_slice(name.as_bytes());
        }
        let (chunk, bytes) = compress(3, &uncompressed);
        manifest.request_chunks[0] = chunk;
        objects[0].bytes = bytes;
        let bytes = objects.into_iter().map(|object| object.bytes).collect();
        assert_eq!(
            manifest
                .decode_objects(bytes, &definitions(&workspace), &dimensions)
                .err(),
            Some(SnapshotError::Corrupt {
                path: "requests/000000".to_owned()
            })
        );
    }

    #[test]
    fn a_measure_listed_two_times_in_the_manifest_is_an_error() {
        let workspace = workspace();
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let mut doubled = snapshot(&workspace);
        let first = doubled.inputs[0].clone();
        let ulid = first.ulid();
        doubled.inputs.push(first);
        let (objects, manifest) = encode(&doubled);
        assert_eq!(
            read_back(manifest, objects, &definitions(&workspace), &dimensions).err(),
            Some(SnapshotError::Definitions(ulid))
        );
    }

    #[test]
    fn a_varint_longer_than_64_bits_is_an_error() {
        let mut bits_64 = vec![0xff; 9];
        bits_64.push(0x01);
        assert_eq!(
            Reader {
                rest: bits_64.into()
            }
            .varint(),
            Ok(u64::MAX)
        );
        let mut bits_65 = vec![0xff; 9];
        bits_65.push(0x02);
        assert_eq!(
            Reader {
                rest: bits_65.into()
            }
            .varint(),
            Err(SnapshotError::Truncated)
        );
    }
}
