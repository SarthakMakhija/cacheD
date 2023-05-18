use std::ops::Div;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rand::{Rng, thread_rng};
use rand_distr::Zipf;
use tokio::runtime::Builder;

use cached::cache::cached::CacheD;
use cached::cache::config::ConfigBuilder;
use cached::cache::types::{TotalCounters, Weight};

const CAPACITY: usize = 2 << 20;
const COUNTERS: TotalCounters = (CAPACITY * 10) as TotalCounters;
const WEIGHT: Weight = CAPACITY as Weight;

const ITEMS: usize = CAPACITY / 3;
const MASK: usize = CAPACITY - 1;

#[cfg(feature = "bench_testable")]
pub fn async_put_single_task(criterion: &mut Criterion) {
    criterion.bench_function("Async Cached.put() | No contention", |bencher| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        bencher.to_async(runtime).iter_custom(|iterations| {
            async move {
                let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());

                let distribution = distribution();
                let mut index = 0;

                let start = Instant::now();
                for _ in 0..iterations {
                    cached.put(distribution[index & MASK], distribution[index & MASK]).unwrap().handle().await;
                    index += 1;
                }
                start.elapsed()
            }
        });
    });
}

pub fn async_put_8_tasks(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    put_parallel(criterion, "Async Cached.put() | 8 tasks", Arc::new(cached), 8, 8);
}

pub fn async_put_16_tasks(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    put_parallel(criterion, "Async Cached.put() | 16 tasks", Arc::new(cached), 8, 16);
}

pub fn async_put_32_tasks(criterion: &mut Criterion) {
    let cached = CacheD::new(ConfigBuilder::new(COUNTERS, CAPACITY, WEIGHT).build());
    put_parallel(criterion, "Async Cached.put() | 32 tasks", Arc::new(cached), 8, 32);
}

#[cfg(feature = "bench_testable")]
pub fn put_parallel(
    criterion: &mut Criterion,
    id: &'static str,
    cached: Arc<CacheD<u64, u64>>,
    thread_count: usize,
    task_count: usize) {
    criterion.bench_function(id, |bencher| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(thread_count)
            .enable_all()
            .build()
            .unwrap();

        bencher.to_async(runtime).iter_custom(|iterations| {
            let cached = cached.clone();
            async move {
                let per_task_iterations = iterations / task_count as u64;
                let mut current_start = 0;
                let mut current_end = current_start + per_task_iterations;
                let distribution = Arc::new(distribution());

                let mut tasks = Vec::new();
                for _task_id in 1..=task_count {
                    let cached = cached.clone();
                    let distribution = distribution.clone();

                    tasks.push(tokio::spawn(async move {
                        let start = Instant::now();
                        for index in current_start..current_end {
                            let key_index = index as usize;
                            cached.put(distribution[key_index & MASK], distribution[key_index & MASK]).unwrap().handle().await;
                        }
                        start.elapsed()
                    }));
                    current_start = current_end;
                    current_end += per_task_iterations;
                }

                let mut total_time = Duration::from_nanos(0);
                for task in tasks {
                    let elapsed = task.await.unwrap();
                    total_time += elapsed;
                }
                total_time.div(task_count as u32)
            }
        });
    });
}


fn distribution() -> Vec<u64> {
    thread_rng().sample_iter(Zipf::new(ITEMS as u64, 1.01).unwrap()).take(CAPACITY).map(|value| value as u64).collect::<Vec<_>>()
}

criterion_group!(benches, async_put_single_task, async_put_8_tasks, async_put_16_tasks, async_put_32_tasks);
criterion_main!(benches);