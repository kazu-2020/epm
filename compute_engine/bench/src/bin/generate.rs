use std::path::PathBuf;
use std::time::Instant;

use datagen::representative::{self, Size};

fn main() -> std::io::Result<()> {
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: generate <directory>"),
    );
    std::fs::create_dir_all(&directory)?;
    let started = Instant::now();
    let workspace = representative::generate(&Size::FULL, bench::SEED);
    eprintln!("generated in {:.1?}", started.elapsed());
    for input in &workspace.inputs {
        println!("{}\t{}", input.definition().name, input.base().num_rows());
        bench::write_measure(&directory, input)?;
    }
    Ok(())
}
