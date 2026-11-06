mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use object_store::memory::InMemory;

const DEFAULT_SEEDS: u64 = 2000;

fn simulate(seed: u64) -> common::Outcome {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime")
        .block_on(common::simulate(Arc::new(InMemory::new()), "ws", seed))
}

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("DST_SEED") {
        return vec![seed.parse().expect("DST_SEED is a number")];
    }
    let count = std::env::var("DST_SEEDS").map_or(DEFAULT_SEEDS, |count| {
        count.parse().expect("DST_SEEDS is a number")
    });
    (0..count).collect()
}

#[test]
fn seeds_keep_the_invariants() {
    let seeds = seeds();
    let started = Instant::now();
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let results: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                let seeds = &seeds;
                scope.spawn(move || {
                    seeds
                        .iter()
                        .skip(thread)
                        .step_by(threads)
                        .map(|&seed| {
                            let outcome = simulate(seed);
                            let counts = common::counts(&outcome);
                            (
                                seed,
                                common::check(&outcome),
                                common::witnesses(&outcome),
                                counts,
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a worker finishes"))
            .collect()
    });
    let mut violated: BTreeMap<&str, (usize, u64, String)> = BTreeMap::new();
    let mut witnessed: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut acknowledged, mut slots) = (0, 0);
    for (seed, violations, witnesses, counts) in results {
        acknowledged += counts.0;
        slots += counts.1;
        for (invariant, detail) in violations {
            let entry = violated.entry(invariant).or_insert((0, seed, detail));
            entry.0 += 1;
        }
        for witness in witnesses {
            *witnessed.entry(witness).or_default() += 1;
        }
    }
    println!("{} seeds in {:.1?}", seeds.len(), started.elapsed());
    println!("{acknowledged} requests acknowledged, {slots} slots written");
    for (witness, count) in &witnessed {
        println!("witnessed {witness} in {count} seeds");
    }
    for (invariant, (count, seed, detail)) in &violated {
        println!("violated {invariant} in {count} seeds, first DST_SEED={seed}: {detail}");
    }
    assert!(
        violated.is_empty(),
        "invariants broke: {:?}",
        violated.keys()
    );
    if seeds.len() >= 100 {
        let expected = [
            "commitAfterHandover",
            "crashAfterApplyBeforeReply",
            "fenceMovedPastOldOwner",
            "oldOwnerStopped",
            "sameBytesResent",
            "storeRejectedPut",
            "twoAcknowledged",
        ];
        for witness in expected {
            assert!(witnessed.contains_key(witness), "no seed reached {witness}");
        }
    }
}

#[test]
fn a_seed_gives_the_same_history_every_time() {
    for seed in 0..300 {
        assert_eq!(simulate(seed), simulate(seed), "seed {seed}");
    }
}
