// Core module that provides a main execution loop and
// the API that can be used to get test data from it.

use rand::{ChaChaRng, Rng, SeedableRng};

use std::cmp::Reverse;
use std::collections::HashMap;
use std::mem;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;

use data::{DataSource, DataStream, Status, TestResult};
use intminimize::minimize_integer;

#[derive(Debug, Clone)]
enum LoopExitReason {
    Complete,
    MaxExamples,
    Shutdown,
}

#[derive(Debug)]
enum LoopCommand {
    RunThis(DataSource),
    Finished(LoopExitReason, MainGenerationLoop),
}

#[derive(Debug)]
struct MainGenerationLoop {
    receiver: Receiver<TestResult>,
    sender: SyncSender<LoopCommand>,
    max_examples: u64,
    random: ChaChaRng,

    best_example: Option<TestResult>,

    valid_examples: u64,
    invalid_examples: u64,
    interesting_examples: u64,
}

type StepResult = Result<(), LoopExitReason>;

impl MainGenerationLoop {
    fn run(mut self) {
        let result = self.loop_body();
        match result {
            // Silent shutdown when the main thread terminates
            Err(LoopExitReason::Shutdown) => (),
            Err(reason) => {
                // Must clone because otherwise it is borrowed.
                let shutdown_sender = self.sender.clone();
                shutdown_sender
                    .send(LoopCommand::Finished(reason, self))
                    .unwrap()
            }
            Ok(_) => panic!("BUG: Generation loop was not supposed to return normally."),
        }
    }

    fn loop_body(&mut self) -> StepResult {
        let interesting_example = self.generate_examples()?;

        let mut shrinker = Shrinker::new(self, interesting_example, |r| {
            r.status == Status::Interesting
        });

        shrinker.run()?;

        return Err(LoopExitReason::Complete);
    }

    fn generate_examples(&mut self) -> Result<TestResult, LoopExitReason> {
        while self.valid_examples < self.max_examples
            && self.invalid_examples < 10 * self.max_examples
        {
            let r = self.random.gen();
            let result = self.execute(DataSource::from_random(r))?;
            if result.status == Status::Interesting {
                return Ok(result);
            }
        }
        return Err(LoopExitReason::MaxExamples);
    }

    fn execute(&mut self, source: DataSource) -> Result<TestResult, LoopExitReason> {
        let result = match self.sender.send(LoopCommand::RunThis(source)) {
            Ok(_) => match self.receiver.recv() {
                Ok(t) => t,
                Err(_) => return Err(LoopExitReason::Shutdown),
            },
            Err(_) => return Err(LoopExitReason::Shutdown),
        };
        match result.status {
            Status::Overflow => (),
            Status::Invalid => self.invalid_examples += 1,
            Status::Valid => self.valid_examples += 1,
            Status::Interesting => {
                self.best_example = Some(result.clone());
                self.interesting_examples += 1;
            }
        }

        Ok(result)
    }
}

struct Shrinker<'owner, Predicate> {
    _predicate: Predicate,
    shrink_target: TestResult,
    changes: u64,
    expensive_passes_enabled: bool,
    main_loop: &'owner mut MainGenerationLoop,
}

impl<'owner, Predicate> Shrinker<'owner, Predicate>
where
    Predicate: Fn(&TestResult) -> bool,
{
    fn new(
        main_loop: &'owner mut MainGenerationLoop,
        shrink_target: TestResult,
        predicate: Predicate,
    ) -> Shrinker<'owner, Predicate> {
        assert!(predicate(&shrink_target));
        Shrinker {
            main_loop: main_loop,
            _predicate: predicate,
            shrink_target: shrink_target,
            changes: 0,
            expensive_passes_enabled: false,
        }
    }

    fn predicate(&mut self, result: &TestResult) -> bool {
        let succeeded = (self._predicate)(result);
        if succeeded
            && (
          // In the presence of writes it may be the case that we thought
          // we were going to shrink this but didn't actually succeed because
          // the written value was used.
          result.record.len() < self.shrink_target.record.len() || (
            result.record.len() == self.shrink_target.record.len()  &&
            result.record < self.shrink_target.record
          )
        ) {
            self.changes += 1;
            self.shrink_target = result.clone();
        }
        succeeded
    }

    fn run(&mut self) -> StepResult {
        let mut prev = self.changes + 1;

        while prev != self.changes {
            prev = self.changes;
            self.adaptive_delete()?;
            self.minimize_individual_blocks()?;
            self.minimize_duplicated_blocks()?;
            if prev == self.changes {
                self.expensive_passes_enabled = true;
            }
            if !self.expensive_passes_enabled {
                continue;
            }

            self.reorder_blocks()?;
            self.lower_and_delete()?;
            self.delete_all_ranges()?;
        }
        Ok(())
    }

    fn lower_and_delete(&mut self) -> StepResult {
        let mut i = 0;
        while i < self.shrink_target.record.len() {
            if self.shrink_target.record[i] > 0 {
                let mut attempt = self.shrink_target.record.clone();
                attempt[i] -= 1;
                let (succeeded, result) = self.execute(&attempt)?;
                if !succeeded && result.record.len() < self.shrink_target.record.len() {
                    let mut j = 0;
                    while j < self.shrink_target.draws.len() {
                        // Having to copy this is an annoying consequence of lexical lifetimes -
                        // if we borrowed it immutably then we'd not be allowed to call self.incorporate
                        // down below. Fortunately these things are tiny structs of integers so it doesn't
                        // really matter.
                        let d = self.shrink_target.draws[j].clone();
                        if d.start > i {
                            let mut attempt2 = attempt.clone();
                            attempt2.drain(d.start..d.end);
                            if self.incorporate(&attempt2)? {
                                break;
                            }
                        }
                        j += 1;
                    }
                }
            }
            i += 1;
        }
        Ok(())
    }

    fn reorder_blocks(&mut self) -> StepResult {
        let mut i = 0;
        while i < self.shrink_target.record.len() {
            let mut j = i + 1;
            while j < self.shrink_target.record.len() {
                assert!(i < self.shrink_target.record.len());
                if self.shrink_target.record[i] == 0 {
                    break;
                }
                if self.shrink_target.record[j] < self.shrink_target.record[i] {
                    let mut attempt = self.shrink_target.record.clone();
                    attempt.swap(i, j);
                    self.incorporate(&attempt)?;
                }
                j += 1;
            }
            i += 1;
        }
        Ok(())
    }

    fn try_delete_range(
        &mut self,
        target: &TestResult,
        i: usize,
        k: usize,
    ) -> Result<bool, LoopExitReason> {
        // Attempts to delete k non-overlapping draws starting from the draw at index i.

        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut j = i;
        while j < target.draws.len() && stack.len() < k {
            let m = target.draws[j].start;
            let n = target.draws[j].end;
            assert!(m < n);
            if m < n && (stack.len() == 0 || stack[stack.len() - 1].1 <= m) {
                stack.push((m, n))
            }
            j += 1;
        }

        let mut attempt = target.record.clone();
        while stack.len() > 0 {
            let (m, n) = stack.pop().unwrap();
            attempt.drain(m..n);
        }

        if attempt.len() >= self.shrink_target.record.len() {
            Ok(false)
        } else {
            self.incorporate(&attempt)
        }
    }

    fn adaptive_delete(&mut self) -> StepResult {
        let mut i = 0;
        let target = self.shrink_target.clone();

        while i < target.draws.len() {
            // This is an adaptive pass loosely modelled after timsort. If
            // little or nothing is deletable here then we don't try any more
            // deletions than the naive greedy algorithm would, but if it looks
            // like we have an opportunity to delete a lot then we try to do so.

            // What we're trying to do is to find a large k such that we can
            // delete k but not k + 1 draws starting from this point, and we
            // want to do that in O(log(k)) rather than O(k) test executions.

            // We try a quite careful sequence of small shrinks here before we
            // move on to anything big. This is because if we try to be
            // aggressive too early on we'll tend to find that we lose out when
            // the example is "nearly minimal".
            if self.try_delete_range(&target, i, 2)? {
                if self.try_delete_range(&target, i, 3)? && self.try_delete_range(&target, i, 4)? {
                    let mut hi = 5;
                    // At this point it looks like we've got a pretty good
                    // opportunity for a long run here. We do an exponential
                    // probe upwards to try and find some k where we can't
                    // delete many intervals. We do this rather than choosing
                    // that upper bound to immediately be large because we
                    // don't really expect k to be huge. If it turns out that
                    // it is, the subsequent example is going to be so tiny that
                    // it doesn't really matter if we waste a bit of extra time
                    // here.
                    while self.try_delete_range(&target, i, hi)? {
                        assert!(hi <= target.draws.len());
                        hi *= 2;
                    }
                    // We now know that we can delete the first lo intervals but
                    // not the first hi. We preserve that property while doing
                    // a binary search to find the point at which we stop being
                    // able to delete intervals.
                    let mut lo = 4;
                    while lo + 1 < hi {
                        let mid = lo + (hi - lo) / 2;
                        if self.try_delete_range(&target, i, mid)? {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                }
            } else {
                self.try_delete_range(&target, i, 1)?;
            }
            // We unconditionally bump i because we have always tried deleting
            // one more example than we succeeded at deleting, so we expect the
            // next example to be undeletable.
            i += 1;
        }
        return Ok(());
    }

    fn delete_all_ranges(&mut self) -> StepResult {
        let mut i = 0;
        while i < self.shrink_target.record.len() {
            let start_length = self.shrink_target.record.len();

            let mut j = i + 1;
            while j < self.shrink_target.record.len() {
                assert!(j > i);
                let mut attempt = self.shrink_target.record.clone();
                attempt.drain(i..j);
                assert!(attempt.len() + (j - i) == self.shrink_target.record.len());
                let deleted = self.incorporate(&attempt)?;
                if !deleted {
                    j += 1;
                }
            }
            if start_length == self.shrink_target.record.len() {
                i += 1;
            }
        }
        Ok(())
    }

    fn try_lowering_value(&mut self, i: usize, v: u64) -> Result<bool, LoopExitReason> {
        if v >= self.shrink_target.record[i] {
            return Ok(false);
        }

        let mut attempt = self.shrink_target.record.clone();
        attempt[i] = v;
        let (succeeded, result) = self.execute(&attempt)?;
        assert!(result.record.len() <= self.shrink_target.record.len());
        let lost_bytes = self.shrink_target.record.len() - result.record.len();
        if !succeeded && result.status == Status::Valid && lost_bytes > 0 {
            attempt.drain(i + 1..i + lost_bytes + 1);
            assert!(attempt.len() + lost_bytes == self.shrink_target.record.len());
            self.incorporate(&attempt)
        } else {
            Ok(succeeded)
        }
    }

    fn minimize_individual_blocks(&mut self) -> StepResult {
        let mut i = 0;

        while i < self.shrink_target.record.len() {
            if !self.shrink_target.written_indices.contains(&i) {
                minimize_integer(self.shrink_target.record[i], |v| {
                    self.try_lowering_value(i, v)
                })?;
            }

            i += 1;
        }

        Ok(())
    }

    fn calc_duplicates(&self) -> Vec<Vec<usize>> {
        assert!(self.shrink_target.record.len() == self.shrink_target.sizes.len());
        let mut duplicates: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
        for (i, (u, v)) in self.shrink_target
            .record
            .iter()
            .zip(self.shrink_target.sizes.iter())
            .enumerate()
        {
            if !self.shrink_target.written_indices.contains(&i) {
                duplicates
                    .entry((*u, *v))
                    .or_insert_with(|| Vec::new())
                    .push(i);
            }
        }

        let mut result: Vec<Vec<usize>> = duplicates
            .drain()
            .filter_map(|(_, elements)| {
                if elements.len() > 1 {
                    Some(elements)
                } else {
                    None
                }
            })
            .collect();
        result.sort_by_key(|v| Reverse(v.len()));
        result
    }

    fn minimize_duplicated_blocks(&mut self) -> StepResult {
        let mut i = 0;
        let mut targets = self.calc_duplicates();
        while i < targets.len() {
            let target = mem::replace(&mut targets[i], Vec::new());
            i += 1;
            assert!(target.len() > 0);
            let v = self.shrink_target.record[target[0]];
            let base = self.shrink_target.record.clone();

            let w = minimize_integer(v, |t| {
                let mut attempt = base.clone();
                for i in &target {
                    attempt[*i] = t
                }
                self.incorporate(&attempt)
            })?;
            if w != v {
                targets = self.calc_duplicates();
            }
        }
        Ok(())
    }

    fn execute(&mut self, buf: &DataStream) -> Result<(bool, TestResult), LoopExitReason> {
        // TODO: Later there will be caching here
        let result = self.main_loop.execute(DataSource::from_vec(buf.clone()))?;
        Ok((self.predicate(&result), result))
    }

    fn incorporate(&mut self, buf: &DataStream) -> Result<bool, LoopExitReason> {
        assert!(
            buf.len() <= self.shrink_target.record.len(),
            "Expected incorporate to not increase length, but buf.len() = {} \
             while shrink target was {}",
            buf.len(),
            self.shrink_target.record.len()
        );
        if buf.len() == self.shrink_target.record.len() {
            assert!(buf < &self.shrink_target.record);
        }
        if self.shrink_target.record.starts_with(buf) {
            return Ok(false);
        }
        let (succeeded, _) = self.execute(buf)?;
        Ok(succeeded)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum EngineState {
    AwaitingCompletion,
    ReadyToProvide,
}

#[derive(Debug)]
pub struct Engine {
    // The next response from the main loop. Once
    // this is set to Some(Finished(_)) it stays that way,
    // otherwise it is cleared on access.
    loop_response: Option<LoopCommand>,

    state: EngineState,

    // Communication channels with the main testing loop
    handle: Option<thread::JoinHandle<()>>,
    receiver: Receiver<LoopCommand>,
    sender: SyncSender<TestResult>,
}

impl Clone for Engine {
    fn clone(&self) -> Engine {
        panic!("BUG: The Engine was unexpectedly cloned");
    }
}

impl Engine {
    pub fn new(max_examples: u64, seed: &[u32]) -> Engine {
        let (send_local, recv_remote) = sync_channel(1);
        let (send_remote, recv_local) = sync_channel(1);

        let main_loop = MainGenerationLoop {
            max_examples: max_examples,
            random: ChaChaRng::from_seed(seed),
            sender: send_remote,
            receiver: recv_remote,
            best_example: None,
            valid_examples: 0,
            invalid_examples: 0,
            interesting_examples: 0,
        };

        let handle = thread::Builder::new()
            .name("Hypothesis main loop".to_string())
            .spawn(move || {
                main_loop.run();
            })
            .unwrap();

        Engine {
            loop_response: None,
            sender: send_local,
            receiver: recv_local,
            handle: Some(handle),
            state: EngineState::ReadyToProvide,
        }
    }

    pub fn mark_finished(&mut self, source: DataSource, status: Status) -> () {
        self.consume_test_result(source.to_result(status))
    }

    pub fn next_source(&mut self) -> Option<DataSource> {
        assert!(self.state == EngineState::ReadyToProvide);
        self.state = EngineState::AwaitingCompletion;

        self.await_loop_response();

        let mut local_result = None;
        mem::swap(&mut local_result, &mut self.loop_response);

        match local_result {
            Some(LoopCommand::RunThis(source)) => return Some(source),
            None => panic!("BUG: Loop response should not be empty at this point"),
            _ => {
                self.loop_response = local_result;
                return None;
            }
        }
    }

    pub fn best_source(&self) -> Option<DataSource> {
        match &self.loop_response {
            &Some(LoopCommand::Finished(
                _,
                MainGenerationLoop {
                    best_example: Some(ref result),
                    ..
                },
            )) => Some(DataSource::from_vec(result.record.clone())),
            _ => None,
        }
    }

    fn consume_test_result(&mut self, result: TestResult) -> () {
        assert!(self.state == EngineState::AwaitingCompletion);
        self.state = EngineState::ReadyToProvide;

        if self.has_shutdown() {
            return ();
        }

        // NB: Deliberately not matching on result. If this fails,
        // that's OK - it means the loop has shut down and when we ask
        // for data from it we'll get its shutdown response.
        let _ = self.sender.send(result);
    }

    pub fn was_unsatisfiable(&self) -> bool {
        match &self.loop_response {
            &Some(LoopCommand::Finished(_, ref main_loop)) => {
                main_loop.interesting_examples == 0 && main_loop.valid_examples == 0
            }
            _ => false,
        }
    }

    fn has_shutdown(&mut self) -> bool {
        match &self.loop_response {
            &Some(LoopCommand::Finished(..)) => true,
            _ => false,
        }
    }

    fn await_thread_termination(&mut self) {
        let mut maybe_handle = None;
        mem::swap(&mut self.handle, &mut maybe_handle);
        if let Some(handle) = maybe_handle {
            if let Err(boxed_msg) = handle.join() {
                // FIXME: This is awful but as far as I can tell this is
                // genuinely the only way to get the actual message out of the
                // panic in the child thread! It's boxed as an Any, and the
                // debug of Any just says "Any". Fortunately the main loop is
                // very much under our control so this doesn't matter too much
                // here, but yuck!
                if let Some(msg) = boxed_msg.downcast_ref::<&str>() {
                    panic!(msg.to_string());
                } else if let Some(msg) = boxed_msg.downcast_ref::<String>() {
                    panic!(msg.clone());
                } else {
                    panic!("BUG: Unexpected panic format in main loop");
                }
            }
        }
    }

    fn await_loop_response(&mut self) -> () {
        if self.loop_response.is_none() {
            match self.receiver.recv() {
                Ok(response) => {
                    self.loop_response = Some(response);
                    if self.has_shutdown() {
                        self.await_thread_termination();
                    }
                }
                Err(_) => {
                    self.await_thread_termination();
                    panic!("BUG: Unexpected silent termination of generation loop.")
                }
            }
        }
    }
}
