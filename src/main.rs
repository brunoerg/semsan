mod corpus_syncer;
mod observers;
mod options;

use clap::Parser;

use std::path::PathBuf;
use std::time::Duration;

use libafl::{
    corpus::{Corpus, InMemoryCorpus, OnDiskCorpus},
    events::{ProgressReporter, SimpleEventManager},
    executors::{DiffExecutor, ForkserverExecutor},
    feedbacks::{
        differential::{DiffFeedback, DiffResult},
        MaxMapFeedback,
    },
    inputs::BytesInput,
    monitors::SimplePrintingMonitor,
    mutators::{havoc_mutations, StdMOptMutator},
    observers::{CanTrack, HitcountsIterableMapObserver, MultiMapObserver, StdMapObserver},
    schedulers::{
        powersched::{PowerQueueScheduler, PowerSchedule},
        IndexesLenTimeMinimizerScheduler,
    },
    stages::{CalibrationStage, StdPowerMutationalStage},
    state::{HasCorpus, HasSolutions, StdState},
    Fuzzer, StdFuzzer,
};
use libafl_bolts::{
    ownedref::OwnedMutSlice,
    rands::StdRand,
    shmem::{ShMem, ShMemProvider, UnixShMemProvider},
    tuples::tuple_list,
};

use crate::corpus_syncer::CorpusSyncer;
use crate::observers::ShMemDifferentialValueObserver;
use crate::options::{Comparator, Options};

const DIFFERENTIAL_VALUE_SHMEM_ID_ENV: &str = "DIFFERENTIAL_VALUE_SHMEM_ID";
const MAX_DIFFERENTIAL_VALUE_SIZE: usize = 32;

fn main() -> std::process::ExitCode {
    let opts = Options::parse();

    const MAX_MAP_SIZE: usize = 2_621_440;
    std::env::set_var("AFL_MAP_SIZE", format!("{}", MAX_MAP_SIZE));

    let mut shmem_provider = UnixShMemProvider::new().unwrap();

    // Create the shared memory that the fuzz harnesses write their execution output to. The output
    // is used as "differential value" to compare program semantics.
    let mut diff_value_shmem = shmem_provider
        .new_shmem(MAX_DIFFERENTIAL_VALUE_SIZE)
        .unwrap();
    diff_value_shmem
        .write_to_env(DIFFERENTIAL_VALUE_SHMEM_ID_ENV)
        .unwrap();

    // Create a differential value observer for each executor.
    let primary_diff_value_observer =
        ShMemDifferentialValueObserver::new("diff-observer-1", unsafe {
            OwnedMutSlice::from_raw_parts_mut(
                diff_value_shmem.as_mut_ptr_of().unwrap(),
                MAX_DIFFERENTIAL_VALUE_SIZE,
            )
        });
    let secondary_diff_value_observer =
        ShMemDifferentialValueObserver::new("diff-observer-2", unsafe {
            OwnedMutSlice::from_raw_parts_mut(
                diff_value_shmem.as_mut_ptr_of().unwrap(),
                MAX_DIFFERENTIAL_VALUE_SIZE,
            )
        });

    let compare_fn = match opts.comparator {
        // Targets behave the same if the outputs are equal
        Comparator::Equal => |output1: &[u8], output2: &[u8]| output1 == output2,
        // Targets behave the same if the primary output is less than the secondary output
        Comparator::LessThan => |output1: &[u8], output2: &[u8]| output1 < output2,
        // Targets behave the same if the primary output is less than or equal to the secondary output
        Comparator::LessThanOrEqual => |output1: &[u8], output2: &[u8]| output1 <= output2,
    };

    // Both observers are combined into a `DiffFeedback` that compares the retrieved values from
    // the two observers described above.
    let mut objective = DiffFeedback::new(
        "diff-value-feedback",
        &primary_diff_value_observer,
        &secondary_diff_value_observer,
        |o1, o2| {
            if compare_fn(o1.last_value(), o2.last_value()) {
                DiffResult::Equal
            } else {
                eprintln!("== ERROR: Semantic Difference");
                eprintln!("primary  : {:?}", o1.last_value());
                eprintln!("secondary: {:?}", o2.last_value());

                DiffResult::Diff
            }
        },
    )
    .unwrap();

    let mut primary_coverage_shmem = shmem_provider.new_shmem(MAX_MAP_SIZE).unwrap();
    let mut secondary_coverage_shmem = shmem_provider.new_shmem(MAX_MAP_SIZE).unwrap();
    let mut coverage_maps: Vec<OwnedMutSlice<'_, u8>> = unsafe {
        vec![
            OwnedMutSlice::from_raw_parts_mut(
                primary_coverage_shmem.as_mut_ptr_of().unwrap(),
                primary_coverage_shmem.len(),
            ),
            OwnedMutSlice::from_raw_parts_mut(
                secondary_coverage_shmem.as_mut_ptr_of().unwrap(),
                secondary_coverage_shmem.len(),
            ),
        ]
    };

    // Create a coverage map observer for each executor
    let primary_map_observer =
        StdMapObserver::from_mut_slice("cov-observer-1", coverage_maps[0].clone());
    let secondary_map_observer =
        StdMapObserver::from_mut_slice("cov-observer-2", coverage_maps[1].clone());

    let primary_executor = ForkserverExecutor::builder()
        .program(PathBuf::from(&opts.primary))
        .debug_child(opts.debug)
        .shmem_provider(&mut shmem_provider)
        .coverage_map_size(MAX_MAP_SIZE)
        .timeout(Duration::from_millis(opts.timeout))
        .env("__AFL_SHM_ID", primary_coverage_shmem.id().to_string())
        .env(
            "__AFL_SHM_ID_SIZE",
            primary_coverage_shmem.len().to_string(),
        )
        .env(
            "LD_PRELOAD",
            std::env::var("SEMSAN_PRIMARY_LD_PRELOAD").unwrap_or(String::new()),
        )
        .is_persistent(true)
        .build_dynamic_map(
            primary_map_observer,
            tuple_list!(primary_diff_value_observer),
        )
        .unwrap();

    let secondary_executor = ForkserverExecutor::builder()
        .program(PathBuf::from(&opts.secondary))
        .debug_child(opts.debug)
        .shmem_provider(&mut shmem_provider)
        .coverage_map_size(MAX_MAP_SIZE)
        .timeout(Duration::from_millis(opts.timeout))
        .env("__AFL_SHM_ID", secondary_coverage_shmem.id().to_string())
        .env(
            "__AFL_SHM_ID_SIZE",
            secondary_coverage_shmem.len().to_string(),
        )
        .env(
            "LD_PRELOAD",
            std::env::var("SEMSAN_SECONDARY_LD_PRELOAD").unwrap_or(String::new()),
        )
        .is_persistent(true)
        .build_dynamic_map(
            secondary_map_observer,
            tuple_list!(secondary_diff_value_observer),
        )
        .unwrap();

    // Resize the coverage maps according to the dynamic map size determined by the executors
    coverage_maps[0].truncate(primary_executor.coverage_map_size().unwrap());

    let secondary_map_size = if opts.no_secondary_coverage {
        0
    } else {
        secondary_executor.coverage_map_size().unwrap()
    };
    coverage_maps[1].truncate(secondary_map_size);

    // Combine both coverage maps as feedback
    let diff_map_observer = HitcountsIterableMapObserver::new(MultiMapObserver::differential(
        "combined-coverage",
        coverage_maps,
    ))
    .track_indices();
    let mut coverage_feedback = MaxMapFeedback::new(&diff_map_observer);

    let calibration_stage = CalibrationStage::new(&coverage_feedback);

    let mut state = StdState::new(
        StdRand::with_seed(libafl_bolts::current_nanos()),
        InMemoryCorpus::<BytesInput>::new(),
        OnDiskCorpus::new(PathBuf::from(&opts.solutions)).unwrap(),
        &mut coverage_feedback,
        &mut objective,
    )
    .unwrap();

    let scheduler = IndexesLenTimeMinimizerScheduler::new(
        &diff_map_observer,
        PowerQueueScheduler::new(&mut state, &diff_map_observer, PowerSchedule::FAST),
    );
    let mut fuzzer = StdFuzzer::new(scheduler, coverage_feedback, objective);

    // Combine the primary and secondary executor into a `DiffExecutor`.
    let mut executor = DiffExecutor::new(
        primary_executor,
        secondary_executor,
        tuple_list!(diff_map_observer),
    );

    let mut mgr = SimpleEventManager::new(SimplePrintingMonitor::new());

    let mut corpus_syncer = CorpusSyncer::new(Duration::from_secs(opts.foreign_sync_interval));

    corpus_syncer.sync(
        &mut state,
        &mut fuzzer,
        &mut executor,
        &mut mgr,
        &[PathBuf::from(&opts.seeds)],
    );

    println!("Loaded {} initial inputs", state.corpus().count());

    let mutator = StdMOptMutator::new(&mut state, havoc_mutations(), 7, 5).unwrap();

    let mut stages = tuple_list!(calibration_stage, StdPowerMutationalStage::new(mutator));

    loop {
        mgr.maybe_report_progress(&mut state, std::time::Duration::from_secs(15))
            .unwrap();
        fuzzer
            .fuzz_one(&mut stages, &mut executor, &mut state, &mut mgr)
            .expect("Error in the fuzzing loop");

        if let Some(foreign_corpus) = opts.foreign_corpus.as_ref() {
            corpus_syncer.sync(
                &mut state,
                &mut fuzzer,
                &mut executor,
                &mut mgr,
                &[PathBuf::from(foreign_corpus)],
            );
        }

        if !opts.ignore_solutions && state.solutions().count() != 0 {
            return std::process::ExitCode::from(opts.solution_exit_code);
        }
    }
}
