use std::sync::Arc;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use rand::{Rng, thread_rng};
use rand_distr::Zipf;

use cached::cache::cached::CacheD;
use cached::cache::config::ConfigBuilder;
use cached::cache::types::{TotalCounters, Weight};

use crate::benchmarks::common::execute_parallel;

const CAPACITY: usize = 2 << 14;
const COUNTERS: TotalCounters = (CAPACITY * 10) as TotalCounters;
const WEIGHT: Weight = CAPACITY as Weight;

const ITEMS: usize = CAPACITY / 3;
const MASK: usize = CAPACITY - 1;

#[cfg(feature = "bench_testable")]
pub fn get_single_threaded(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    let distribution = distribution();

    preload_cache(&cached, &distribution);

    let mut index = 0;
    criterion.bench_function("Cached.get() | No contention", |bencher| {
        bencher.iter_custom(|iterations| {
            let start = Instant::now();
            for _ in 0..iterations {
                let _ = cached.get(&distribution[index & MASK]);
                index += 1;
            }
            start.elapsed()
        });
    });
}

#[cfg(feature = "bench_testable")]
pub fn get_8_threads(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    let distribution = distribution();

    preload_cache(&cached, &distribution);
    execute_parallel(criterion, "Cached.get() | 8 threads", prepare_execution_block(cached, Arc::new(distribution)), 8);
}

#[cfg(feature = "bench_testable")]
pub fn get_16_threads(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    let distribution = distribution();

    preload_cache(&cached, &distribution);
    execute_parallel(criterion, "Cached.get() | 16 threads", prepare_execution_block(cached, Arc::new(distribution)), 16);
}

#[cfg(feature = "bench_testable")]
pub fn get_32_threads(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    let distribution = distribution();

    preload_cache(&cached, &distribution);
    execute_parallel(criterion, "Cached.get() | 32 threads", prepare_execution_block(cached, Arc::new(distribution)), 32);
}

fn preload_cache(cached: &CacheD<u64, u64>, distribution: &Vec<u64>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            setup(&cached, &distribution).await;
        });
}

fn prepare_execution_block(cached: CacheD<u64, u64>, distribution: Arc<Vec<u64>>) -> Arc<impl Fn(u64) -> () + Send + Sync + 'static> {
    Arc::new(move |index| {
        let key_index = index as usize;
        let _ = cached.get(&distribution[key_index & MASK]);
    })
}

async fn setup(cached: &CacheD<u64, u64>, distribution: &Vec<u64>) {
    for element in distribution {
        cached.put(*element, *element).unwrap().handle().await;
    }
}

fn distribution() -> Vec<u64> {
    thread_rng().sample_iter(Zipf::new(ITEMS as u64, 1.01).unwrap()).take(CAPACITY).map(|value| value as u64).collect::<Vec<_>>()
}

criterion_group!(benches, get_single_threaded, get_8_threads, get_16_threads, get_32_threads);
criterion_main!(benches);