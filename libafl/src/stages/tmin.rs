//! The [`StdTMinMutationalStage`] is a stage which will attempt to minimize corpus entries.

use alloc::{
    borrow::{Cow, ToOwned},
    string::ToString,
};
use core::{borrow::BorrowMut, fmt::Debug, hash::Hash, marker::PhantomData};

use ahash::RandomState;
use libafl_bolts::{
    tuples::{Handle, Handled, MatchName, MatchNameRef},
    HasLen, Named,
};
use serde::Serialize;

#[cfg(feature = "track_hit_feedbacks")]
use crate::feedbacks::premature_last_result_err;
#[cfg(feature = "introspection")]
use crate::monitors::PerfFeature;
use crate::{
    corpus::{Corpus, HasCurrentCorpusId, Testcase},
    events::EventFirer,
    executors::{ExitKind, HasObservers},
    feedbacks::{Feedback, FeedbackFactory, HasObserverHandle, StateInitializer},
    inputs::Input,
    mark_feature_time,
    mutators::{MutationResult, Mutator},
    observers::{MapObserver, ObserversTuple},
    schedulers::RemovableScheduler,
    stages::{
        mutational::{MutatedTransform, MutatedTransformPost},
        ExecutionCountRestartHelper, Stage,
    },
    start_timer,
    state::{
        HasCorpus, HasCurrentTestcase, HasExecutions, HasMaxSize, HasSolutions,
        MaybeHasClientPerfMonitor,
    },
    Error, ExecutesInput, ExecutionProcessor, HasFeedback, HasMetadata, HasNamedMetadata,
    HasScheduler,
};

/// The default corpus entry minimising mutational stage
#[derive(Clone, Debug)]
pub struct StdTMinMutationalStage<E, EM, F, FF, M, S, Z> {
    /// The name
    name: Cow<'static, str>,
    /// The mutator(s) this stage uses
    mutator: M,
    /// The factory
    factory: FF,
    /// The runs (=iterations) we are supposed to do
    runs: usize,
    /// The progress helper for this stage, keeping track of resumes after timeouts/crashes
    restart_helper: ExecutionCountRestartHelper,
    phantom: PhantomData<(E, EM, F, S, Z)>,
}

impl<E, EM, F, FF, M, S, Z> Stage<E, EM, S, Z> for StdTMinMutationalStage<E, EM, F, FF, M, S, Z>
where
    Z: HasScheduler<<S::Corpus as Corpus>::Input, S>
        + ExecutionProcessor<EM, <S::Corpus as Corpus>::Input, E::Observers, S>
        + ExecutesInput<E, EM, <S::Corpus as Corpus>::Input, S>
        + HasFeedback,
    Z::Scheduler: RemovableScheduler<<S::Corpus as Corpus>::Input, S>,
    E: HasObservers,
    E::Observers: ObserversTuple<<S::Corpus as Corpus>::Input, S> + Serialize,
    EM: EventFirer<<S::Corpus as Corpus>::Input, S>,
    FF: FeedbackFactory<F, E::Observers>,
    F: Feedback<EM, <S::Corpus as Corpus>::Input, E::Observers, S>,
    S: HasMetadata
        + HasExecutions
        + HasSolutions
        + HasCorpus
        + HasMaxSize
        + HasNamedMetadata
        + HasCurrentCorpusId
        + MaybeHasClientPerfMonitor,
    Z::Feedback: Feedback<EM, <S::Corpus as Corpus>::Input, E::Observers, S>,
    M: Mutator<<S::Corpus as Corpus>::Input, S>,
    <<S as HasCorpus>::Corpus as Corpus>::Input: Input + Hash + HasLen,
{
    fn should_restart(&mut self, state: &mut S) -> Result<bool, Error> {
        self.restart_helper.should_restart(state, &self.name)
    }

    fn clear_progress(&mut self, state: &mut S) -> Result<(), Error> {
        self.restart_helper.clear_progress(state, &self.name)
    }

    fn perform(
        &mut self,
        fuzzer: &mut Z,
        executor: &mut E,
        state: &mut S,
        manager: &mut EM,
    ) -> Result<(), Error> {
        self.perform_minification(fuzzer, executor, state, manager)?;

        #[cfg(feature = "introspection")]
        state.introspection_monitor_mut().finish_stage();

        Ok(())
    }
}

impl<E, EM, F, FF, M, S, Z> FeedbackFactory<F, E::Observers>
    for StdTMinMutationalStage<E, EM, F, FF, M, S, Z>
where
    E: HasObservers,
    FF: FeedbackFactory<F, E::Observers>,
{
    fn create_feedback(&self, ctx: &E::Observers) -> F {
        self.factory.create_feedback(ctx)
    }
}

impl<E, EM, F, FF, M, S, Z> Named for StdTMinMutationalStage<E, EM, F, FF, M, S, Z> {
    fn name(&self) -> &Cow<'static, str> {
        &self.name
    }
}

/// The counter for giving this stage unique id
static mut TMIN_STAGE_ID: usize = 0;
/// The name for tmin stage
pub static TMIN_STAGE_NAME: &str = "tmin";

impl<E, EM, F, FF, M, S, Z> StdTMinMutationalStage<E, EM, F, FF, M, S, Z>
where
    Z: HasScheduler<<S::Corpus as Corpus>::Input, S>
        + ExecutionProcessor<EM, <S::Corpus as Corpus>::Input, E::Observers, S>
        + ExecutesInput<E, EM, <S::Corpus as Corpus>::Input, S>
        + HasFeedback,
    Z::Scheduler: RemovableScheduler<<S::Corpus as Corpus>::Input, S>,
    E: HasObservers,
    E::Observers: ObserversTuple<<S::Corpus as Corpus>::Input, S> + Serialize,
    EM: EventFirer<<S::Corpus as Corpus>::Input, S>,
    FF: FeedbackFactory<F, E::Observers>,
    F: Feedback<EM, <S::Corpus as Corpus>::Input, E::Observers, S>,
    S: HasMetadata
        + HasExecutions
        + HasSolutions
        + HasCorpus
        + HasMaxSize
        + HasNamedMetadata
        + HasCurrentTestcase
        + HasCurrentCorpusId
        + MaybeHasClientPerfMonitor,
    Z::Feedback: Feedback<EM, <S::Corpus as Corpus>::Input, E::Observers, S>,
    M: Mutator<<S::Corpus as Corpus>::Input, S>,
    <S::Corpus as Corpus>::Input: Hash + HasLen + Input,
{
    /// The list of mutators, added to this stage (as mutable ref)
    #[inline]
    fn mutator_mut(&mut self) -> &mut M {
        &mut self.mutator
    }

    /// Gets the number of iterations from a fixed number of runs
    fn iterations(&self, _state: &mut S) -> usize {
        self.runs
    }

    /// Runs this (mutational) stage for new objectives
    fn perform_minification(
        &mut self,
        fuzzer: &mut Z,
        executor: &mut E,
        state: &mut S,
        manager: &mut EM,
    ) -> Result<(), Error> {
        let Some(base_corpus_id) = state.current_corpus_id()? else {
            return Err(Error::illegal_state(
                "state is not currently processing a corpus index",
            ));
        };

        let orig_max_size = state.max_size();
        // basically copy-pasted from mutational.rs
        let num = self
            .iterations(state)
            .saturating_sub(usize::try_from(self.execs_since_progress_start(state)?)?);

        // If num is negative, then quit.
        if num == 0 {
            return Ok(());
        }

        start_timer!(state);
        let transformed = <S::Corpus as Corpus>::Input::try_transform_from(
            state.current_testcase_mut()?.borrow_mut(),
            state,
        )?;
        let mut base = state.current_input_cloned()?;
        // potential post operation if base is replaced by a shorter input
        let mut base_post = None;
        let base_hash = RandomState::with_seeds(0, 0, 0, 0).hash_one(&base);
        mark_feature_time!(state, PerfFeature::GetInputFromCorpus);

        fuzzer.execute_input(state, executor, manager, &base)?;
        let observers = executor.observers();

        let mut feedback = self.create_feedback(&*observers);

        let mut i = 0;
        loop {
            if i >= num {
                break;
            }

            let mut next_i = i + 1;
            let mut input_transformed = transformed.clone();

            let before_len = base.len();

            state.set_max_size(before_len);

            start_timer!(state);
            let mutated = self.mutator_mut().mutate(state, &mut input_transformed)?;
            mark_feature_time!(state, PerfFeature::Mutate);

            if mutated == MutationResult::Skipped {
                continue;
            }

            let (input, post) = input_transformed.try_transform_into(state)?;
            let corpus_id = if input.len() < before_len {
                // run the input
                let exit_kind = fuzzer.execute_input(state, executor, manager, &input)?;
                let observers = executor.observers();

                // let the fuzzer process this execution -- it's possible that we find something
                // interesting, or even a solution

                // TODO replace if process_execution adds a return value for solution index
                let solution_count = state.solutions().count();
                let corpus_count = state.corpus().count();
                let (_, corpus_id) = fuzzer.evaluate_execution(
                    state,
                    manager,
                    input.clone(),
                    &*observers,
                    &exit_kind,
                    false,
                )?;

                if state.corpus().count() == corpus_count
                    && state.solutions().count() == solution_count
                {
                    // we do not care about interesting inputs!
                    if feedback.is_interesting(state, manager, &input, &*observers, &exit_kind)? {
                        // we found a reduced corpus entry! use the smaller base
                        base = input;
                        base_post = Some(post);

                        // do more runs! maybe we can minify further
                        next_i = 0;
                    }
                }

                corpus_id
            } else {
                // we can't guarantee that the mutators provided will necessarily reduce size, so
                // skip any mutations that actually increase size so we don't waste eval time
                None
            };

            start_timer!(state);
            self.mutator_mut().post_exec(state, corpus_id)?;
            post.post_exec(state, corpus_id)?;
            mark_feature_time!(state, PerfFeature::MutatePostExec);

            i = next_i;
        }

        let new_hash = RandomState::with_seeds(0, 0, 0, 0).hash_one(&base);
        if base_hash != new_hash {
            let exit_kind = fuzzer.execute_input(state, executor, manager, &base)?;
            let observers = executor.observers();
            // assumption: this input should not be marked interesting because it was not
            // marked as interesting above; similarly, it should not trigger objectives
            fuzzer
                .feedback_mut()
                .is_interesting(state, manager, &base, &*observers, &exit_kind)?;
            let mut testcase = Testcase::from(base);
            fuzzer
                .feedback_mut()
                .append_metadata(state, manager, &*observers, &mut testcase)?;
            let prev = state.corpus_mut().replace(base_corpus_id, testcase)?;
            fuzzer
                .scheduler_mut()
                .on_replace(state, base_corpus_id, &prev)?;
            // perform the post operation for the new testcase, e.g. to update metadata.
            // base_post should be updated along with the base (and is no longer None)
            base_post
                .ok_or_else(|| Error::empty_optional("Failed to get the MutatedTransformPost"))?
                .post_exec(state, Some(base_corpus_id))?;
        }

        state.set_max_size(orig_max_size);

        Ok(())
    }

    fn execs_since_progress_start(&mut self, state: &mut S) -> Result<u64, Error> {
        self.restart_helper
            .execs_since_progress_start(state, &self.name)
    }
}

impl<E, EM, F, FF, M, S, Z> StdTMinMutationalStage<E, EM, F, FF, M, S, Z> {
    /// Creates a new minimizing mutational stage that will minimize provided corpus entries
    pub fn new(mutator: M, factory: FF, runs: usize) -> Self {
        // unsafe but impossible that you create two threads both instantiating this instance
        let stage_id = unsafe {
            let ret = TMIN_STAGE_ID;
            TMIN_STAGE_ID += 1;
            ret
        };
        Self {
            name: Cow::Owned(TMIN_STAGE_NAME.to_owned() + ":" + stage_id.to_string().as_str()),
            mutator,
            factory,
            runs,
            restart_helper: ExecutionCountRestartHelper::default(),
            phantom: PhantomData,
        }
    }
}

/// A feedback which checks if the hash of the currently observed map is equal to the original hash
/// provided
#[derive(Clone, Debug)]
pub struct MapEqualityFeedback<C, M, S> {
    name: Cow<'static, str>,
    map_ref: Handle<C>,
    orig_hash: u64,
    #[cfg(feature = "track_hit_feedbacks")]
    // The previous run's result of `Self::is_interesting`
    last_result: Option<bool>,
    phantom: PhantomData<(M, S)>,
}

impl<C, M, S> Named for MapEqualityFeedback<C, M, S> {
    fn name(&self) -> &Cow<'static, str> {
        &self.name
    }
}

impl<C, M, S> HasObserverHandle for MapEqualityFeedback<C, M, S> {
    type Observer = C;

    fn observer_handle(&self) -> &Handle<Self::Observer> {
        &self.map_ref
    }
}

impl<C, M, S> StateInitializer<S> for MapEqualityFeedback<C, M, S> {}

impl<C, EM, I, M, OT, S> Feedback<EM, I, OT, S> for MapEqualityFeedback<C, M, S>
where
    M: MapObserver,
    C: AsRef<M>,
    OT: MatchName,
{
    fn is_interesting(
        &mut self,
        _state: &mut S,
        _manager: &mut EM,
        _input: &I,
        observers: &OT,
        _exit_kind: &ExitKind,
    ) -> Result<bool, Error> {
        let obs = observers
            .get(self.observer_handle())
            .expect("Should have been provided valid observer name.");
        let res = obs.as_ref().hash_simple() == self.orig_hash;
        #[cfg(feature = "track_hit_feedbacks")]
        {
            self.last_result = Some(res);
        }
        Ok(res)
    }
    #[cfg(feature = "track_hit_feedbacks")]
    fn last_result(&self) -> Result<bool, Error> {
        self.last_result.ok_or(premature_last_result_err())
    }
}

/// A feedback factory for ensuring that the maps for minimized inputs are the same
#[derive(Debug, Clone)]
pub struct MapEqualityFactory<C, M, S> {
    map_ref: Handle<C>,
    phantom: PhantomData<(C, M, S)>,
}

impl<C, M, S> MapEqualityFactory<C, M, S>
where
    M: MapObserver,
    C: AsRef<M> + Handled,
{
    /// Creates a new map equality feedback for the given observer
    pub fn new(obs: &C) -> Self {
        Self {
            map_ref: obs.handle(),
            phantom: PhantomData,
        }
    }
}

impl<C, M, S> HasObserverHandle for MapEqualityFactory<C, M, S> {
    type Observer = C;

    fn observer_handle(&self) -> &Handle<C> {
        &self.map_ref
    }
}

impl<C, M, OT, S> FeedbackFactory<MapEqualityFeedback<C, M, S>, OT> for MapEqualityFactory<C, M, S>
where
    M: MapObserver,
    C: AsRef<M> + Handled,
    OT: ObserversTuple<<S::Corpus as Corpus>::Input, S>,
    S: HasCorpus,
{
    fn create_feedback(&self, observers: &OT) -> MapEqualityFeedback<C, M, S> {
        let obs = observers
            .get(self.observer_handle())
            .expect("Should have been provided valid observer name.");
        MapEqualityFeedback {
            name: Cow::from("MapEq"),
            map_ref: obs.handle(),
            orig_hash: obs.as_ref().hash_simple(),
            #[cfg(feature = "track_hit_feedbacks")]
            last_result: None,
            phantom: PhantomData,
        }
    }
}
