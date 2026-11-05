use std::path::PathBuf;
use std::time::Instant;

use datagen::representative::{self, Size};

fn main() -> std::io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let usage = "usage: generate <directory> [scale]";
    let directory = PathBuf::from(arguments.next().expect(usage));
    let scale: usize = arguments
        .next()
        .map_or(1, |scale| scale.parse().expect(usage));
    let size = Size {
        quantity_cells: Size::FULL.quantity_cells * scale,
        ..Size::FULL
    };
    std::fs::create_dir_all(&directory)?;
    let started = Instant::now();
    let workspace = representative::generate(&size, bench::SEED);
    eprintln!("generated in {:.1?}", started.elapsed());
    for input in &workspace.inputs {
        println!("{}\t{}", input.definition().name, input.len());
        bench::write_measure(&directory, input)?;
    }
    Ok(())
}
