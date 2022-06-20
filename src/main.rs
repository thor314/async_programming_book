use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use futures::join;

// Sleep for a Duration, then become ready
pub struct TimerFuture {
    shared_state: Arc<Mutex<SharedState>>,
    name: String,
}

/// Shared state between the future and the waiting thread
struct SharedState {
    /// Whether or not the sleep time has elapsed
    completed: bool,

    /// The waker for the task that `TimerFuture` is running on.
    /// The thread can use this after setting `completed = true` to tell
    /// `TimerFuture`'s task to wake up, see that `completed = true`, and
    /// move forward.
    waker: Option<Waker>,
}

impl Future for TimerFuture {
    type Output = ();
    // meat! lives here. Hunt for the asynchronous buffalo.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the shared state, and examine it.
        // Look at the shared state to see if the timer has already completed.
        let mut shared_state = self.shared_state.lock().unwrap();

        if shared_state.completed {
            println!("*4-ish* polled ready: {}",self.name);
            Poll::Ready(()) // yeet it up
        } else {
            // Set waker so that the thread can wake up the current task
            // when the timer has completed, ensuring that the future is polled
            // again and sees that `completed = true`.
            //
            // It's tempting to do this once rather than repeatedly cloning
            // the waker each time. However, the `TimerFuture` can move between
            // tasks on the executor, which could cause a stale waker pointing
            // to the wrong task, preventing `TimerFuture` from waking up
            // correctly.
            //
            // N.B. it's possible to check for this using the `Waker::will_wake`
            // function, but we omit that here to keep things simple.
            println!("*4-ish* polled task {}, wasn't ready", self.name);
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
// finally, implement some actual functionality.
impl TimerFuture {
    pub fn new(dur: Duration, s: String) -> Self {
        // create the shared state internal
        let shared = Arc::new(Mutex::new(SharedState {
            completed: false,
            waker: None,
        }));

        // clone it, and move it into a thread
        let thread_shared = shared.clone();
        let sp = s.clone();
        println!(
            "*3* sanity check outside thread, waker is None: {}: {:?}",
            s,
            shared.lock().unwrap().waker.clone()
        );

        thread::spawn(move || {
            // do the sleeping. The executor is going to poll right about now, but may not poll until after after this println.
            // Note that Polling updates the waker field from None to Some.
            println!(
                "*4-ish* from thread before sleep, waker {} is (unpredictable): {:?}",
                sp,
                thread_shared.lock().unwrap().waker.clone()
            );
            thread::sleep(dur);
            // we're done, waker has been set by the executor. We're going to call it.
            println!(
                "*5-ish* from thread after sleep, waker is Some: {}: {:?}",
                sp,
                thread_shared.lock().unwrap().waker.clone()
            );
            let mut shared_state = thread_shared.lock().unwrap();
            shared_state.completed = true;
            if let Some(waker) = shared_state.waker.take() {
                println!("*5-ish* awoke {}: {waker:?}", sp.clone());
                waker.wake()
            } else {
                panic!("this never happens in this example.");
            }
        });
        TimerFuture {
            shared_state: shared,
            name: s,
        }
    }
}

// Okay, but we don't actually have a way to consume these futures yet. So write an executor.
// A what?
// An executor. it takes a Future, and runs it to completion. Futures, like teenagers are lazy. Executors make em move, like high school gym teachers. We're going to write a gym teacher. To do that, we need a utility from the futures crate.
pub mod executor {
    use {
        // The timer we wrote in the previous section:
        futures::{
            // a future in a box! and some convenient adaptors.
            future::BoxFuture,

            task::{waker_ref, ArcWake},
        },
        std::{
            future::Future,
            sync::mpsc::{sync_channel, Receiver, SyncSender},
            sync::{Arc, Mutex},
            task::Context,
        },
    };

    /// Task executor that receives tasks off a channel, and runs them. This is our "task manager."
    pub struct Executor {
        ready_queue: Receiver<Arc<Task>>,
    }

    /// Stuff we'll hand to the executor to get DONE. The executor is going to poll this guy.
    pub struct Task {
        // The future ain't here yet? It might be soon. We don't actually need this Mutex, since we'll only have one thread executing tasks at a time. But compiler friend doesn't know that, so the Mutex proves thread safety. In production, an `UnsafeCell` would be used to eliminate that overhead.
        future: Mutex<Option<BoxFuture<'static, ()>>>,
        task_sender: SyncSender<Arc<Task>>, // woah dude, recurse. Handle to place the task back onto the task queue.
    }

    /// Convenience struct for spawnning new tasks to pass to Executor guy.
    #[derive(Clone)]
    pub struct Spawner {
        // a SyncSender is the sending half of a channel. Someone's gonna receive it, and that someone's a gym teacher.
        task_sender: SyncSender<Arc<Task>>,
    }

    // we create the executor and the spawner at the same time.
    pub fn new_executor_and_spawner() -> (Executor, Spawner) {
        // don't let the number of queued tasks consume all my CPUs please
        const MAX_QUEUED: usize = 10_000;
        // create a new sync bounded channel. I think this is equivalent to mpsc::channel, with a parameter for max queue size.
        let (task_sender, ready_queue) = sync_channel(MAX_QUEUED);
        (Executor { ready_queue }, Spawner { task_sender })
    }

    impl Spawner {
        pub fn spawn(&self, future: impl Future<Output = ()> + 'static + Send) {
            // let future = future.boxed();
            //spawn the task, creating an atomic reference to pass it around. The task itself has to be pinned to memory.
            let task = Arc::new(Task::new(future, &self.task_sender));
            // send it to
            self.task_sender.send(task).unwrap();
        }
    }
    impl Task {
        fn new(
            future: impl Future<Output = ()> + 'static + Send,
            task_sender: &SyncSender<Arc<Task>>,
        ) -> Self {
            Self {
                // box it, pin it
                future: Mutex::new(Some(Box::pin(future))),
                task_sender: task_sender.clone(),
            }
        }
    }
    // We want to be able to turn our tasks into Wakers. Wakers either bottom, or schedule a task to be polled again.
    impl ArcWake for Task {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            println!("*5-ish* arcwake called from task. This gets called after the shared_state waker gets taken.");
            let cloned = arc_self.clone();
            arc_self
                .task_sender
                .send(cloned)
                .expect("too many tasks queued");
        }
    }
    // finally, our Executor will poll tasks by picking up the waker-ified tasks sent over the channel.
    impl Executor {
        pub fn run(&self) {
            let mut count = 0;
            println!("*0* executor run entrypoint");
            // THERE IS A TASK IN THE READY QUEUE, MAGGOT
            while let Ok(task) = self.ready_queue.recv() {
                println!(
                    "*uhh* Arcwake handed me a task! Calling all wakers. Count of task so far: {:?}",
                    count
                );
                count += 1;
                // take the future and poll it
                let mut future_slot = task.future.lock().unwrap();
                if let Some(mut future) = future_slot.take() {
                    // Use ArcWake to get a Waker from our task
                    let waker = waker_ref(&task);
                    // use the waker to construct a context to poll our future from
                    let context = &mut Context::from_waker(&*waker);
                    // Poll the task with. If it's pending, put it back to run it again.
                    if future.as_mut().poll(context).is_pending() {
                        // put it back
                        *future_slot = Some(future)
                    }
                }
            }
        }
    }
    // experiments:
    // remove mutex in task::future
    // try removing spawn's trait bounds
}

fn main() {
    use executor::*;
    let (executor, spawner) = new_executor_and_spawner();

    // spawn a task or several tasks, and send them to the executor.
    spawner.spawn(async {
        // do some things
        println!("*2* begin polling some async tasks...");
        // blocking:
        // TimerFuture::new(Duration::new(2, 0)).await;
        // println!("two more");
        // TimerFuture::new(Duration::new(1, 0)).await;
        // println!("one more");
        // TimerFuture::new(Duration::new(1, 0)).await;

        // non-blocking:
        let t = TimerFuture::new(Duration::new(1, 0), "1".into());
        let t2 = TimerFuture::new(Duration::new(2, 0), "2".into());
        let t3 = TimerFuture::new(Duration::new(3, 0), "3".into());
        join!(t, t2, t3); // non-blocking alternative: don't block the Executor
        println!("done");
    });

    // drop the spawner, so the executor knows it won't receive more tasks.
    // if this line is not included, the last println never runs, and the executor will block indefinitely, after the tasks are managed.
    drop(spawner);

    // run until empty queue
    executor.run();

    println!("It has run!");
}
