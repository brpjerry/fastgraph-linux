//! The worker thread: file work (the boot migration, recovery writes,
//! saves, exports and folder listing) runs on it one at a time in the order
//! it was asked for, so a later write never overtakes an earlier one. Each
//! result comes back as a message.
use crate::app::Message;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// A piece of work and the message it reports, if any.
pub type Job = Box<dyn FnOnce() -> Option<Message> + Send>;

pub struct Worker {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Run jobs and pass their messages to `reply`.
    pub fn start(reply: impl Fn(Message) + Send + 'static) -> Worker {
        let (jobs, queue) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("fastgraph-worker".into())
            .spawn(move || {
                for job in queue {
                    // A panicking job must not end the thread: saves, recovery
                    // and exports behind it still need to run.
                    match std::panic::catch_unwind(AssertUnwindSafe(job)) {
                        Ok(Some(message)) => reply(message),
                        Ok(None) => {}
                        Err(why) => {
                            let text = why
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| why.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "unknown panic".into());
                            reply(Message::WorkerPanicked(text));
                        }
                    }
                }
            })
            .expect("spawn the worker thread");
        Worker {
            jobs: Some(jobs),
            thread: Some(thread),
        }
    }

    /// Queue `job` after every one before it.
    pub fn submit(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    /// Run every queued job, then end the thread.
    pub fn finish(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Wait until every job queued so far has run.
    #[cfg(test)]
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        self.submit(Box::new(move || {
            let _ = done.send(());
            None
        }));
        let _ = wait.recv();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_panicking_job_does_not_stop_the_worker() {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::start(move |m| {
            let _ = tx.send(m);
        });
        worker.submit(Box::new(|| panic!("boom")));
        worker.submit(Box::new(|| Some(Message::Tick)));
        worker.flush();
        let first = rx.recv().unwrap();
        assert!(
            matches!(first, Message::WorkerPanicked(ref t) if t == "boom"),
            "{first:?}"
        );
        assert!(matches!(rx.recv().unwrap(), Message::Tick));
    }
}
