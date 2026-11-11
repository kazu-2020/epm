use bytes::Bytes;
use change_log::{Slot, Start};
use dimension::Dimension;
use measure::MeasureDefinition;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use workspace::Workspace;

use crate::snapshot::{Manifest, Snapshot};
use crate::{Engine, FormulaError};

pub async fn restore(
    store: &dyn ObjectStore,
    name: &str,
    workspace: Workspace,
) -> Result<(Engine, Start), FormulaError> {
    let folder = Path::from(name).join("snapshot");
    // Not an error: without a list, the owner reads the whole log, which gives the same state.
    let mut found = store
        .list_with_delimiter(Some(&folder))
        .await
        .map_or_else(|_| Vec::new(), |listed| listed.common_prefixes);
    found.sort();
    let definitions: Vec<MeasureDefinition> = workspace
        .inputs
        .iter()
        .map(|input| input.definition().clone())
        .collect();
    let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
    for snapshot in found.iter().rev() {
        // Not an alarm: a snapshot without a manifest is not written yet, and an unreadable one is only a lost cache.
        let Some(Snapshot { mut start, inputs }) =
            read(store, snapshot, &definitions, &dimensions).await
        else {
            continue;
        };
        if cfg!(feature = "snapshot_start_after") {
            start.slot = Slot(start.slot.0 + 1);
        }
        let Workspace {
            dimensions,
            formulas,
            ..
        } = workspace;
        let resumed = Workspace {
            dimensions,
            inputs,
            formulas,
        };
        return Ok((Engine::resume(resumed, &start)?, start));
    }
    Ok((Engine::new(workspace)?, Start::default()))
}

async fn read(
    store: &dyn ObjectStore,
    folder: &Path,
    definitions: &[MeasureDefinition],
    dimensions: &[&Dimension],
) -> Option<Snapshot> {
    let manifest = Manifest::decode(get(store, folder, "manifest").await?).ok()?;
    let mut objects = Vec::new();
    for path in manifest.paths() {
        objects.push(get(store, folder, &path).await?);
    }
    manifest
        .decode_objects(objects, definitions, dimensions)
        .ok()
}

async fn get(store: &dyn ObjectStore, folder: &Path, name: &str) -> Option<Bytes> {
    let path = Path::from_iter(folder.parts().chain(Path::from(name).parts()));
    store.get(&path).await.ok()?.bytes().await.ok()
}
