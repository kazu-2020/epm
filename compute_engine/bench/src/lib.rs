use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::FileWriter;
use dimension::{Dimension, DimensionGlobalId};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;

pub const SEED: u64 = 20_261_103;

const ULID_KEY: &str = "ulid";
const NAME_KEY: &str = "name";

pub fn write_measure(directory: &Path, measure: &Measure) -> std::io::Result<()> {
    let definition = measure.definition();
    let schema = Arc::new(definition.schema().with_metadata(HashMap::from([
        (ULID_KEY.to_string(), definition.ulid.0.to_string()),
        (NAME_KEY.to_string(), definition.name.clone()),
    ])));
    let file = File::create(directory.join(format!("{}.arrow", definition.ulid.0)))?;
    let mut writer = FileWriter::try_new(file, &schema).map_err(std::io::Error::other)?;
    let batch = measure
        .compact()
        .to_record_batch()
        .with_schema(schema)
        .map_err(std::io::Error::other)?;
    writer.write(&batch).map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)
}

pub fn read_measures(directory: &Path, dimensions: &[&Dimension]) -> std::io::Result<Vec<Measure>> {
    let mut paths: Vec<_> = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "arrow")
    });
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let reader =
                FileReader::try_new(File::open(&path)?, None).map_err(std::io::Error::other)?;
            let schema = reader.schema();
            let metadata = schema.metadata();
            let parse = |text: &str| Ulid::from_string(text).map_err(std::io::Error::other);
            let definition = MeasureDefinition {
                ulid: MeasureGlobalId(parse(&metadata[ULID_KEY])?),
                name: metadata[NAME_KEY].clone(),
                axes: schema.fields()[..schema.fields().len() - 1]
                    .iter()
                    .map(|field| parse(field.name()).map(DimensionGlobalId))
                    .collect::<Result<_, _>>()?,
            };
            let batches = reader
                .collect::<Result<Vec<_>, _>>()
                .map_err(std::io::Error::other)?;
            let [batch] = &batches[..] else {
                return Err(std::io::Error::other("a measure file holds one batch"));
            };
            Measure::try_new(definition, batch.clone(), dimensions).map_err(std::io::Error::other)
        })
        .collect()
}
