//! Tests for the array channel flavor.

extern crate crossbeam_utils;
extern crate new_channel;
extern crate rand;

use std::any::Any;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossbeam_utils::thread::scope;
use new_channel::{bounded, Receiver};
use new_channel::{RecvError, RecvTimeoutError, TryRecvError};
use new_channel::{SendError, SendTimeoutError, TrySendError};
use rand::{thread_rng, Rng};

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
fn smoke() {
    let (s, r) = bounded(1);
    s.send(7).unwrap();
    assert_eq!(r.try_recv(), Ok(7));

    s.send(8).unwrap();
    assert_eq!(r.recv(), Ok(8));

    assert_eq!(r.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(r.recv_timeout(ms(1000)), Err(RecvTimeoutError::Timeout));
}

#[test]
fn try_recv() {
    let (s, r) = bounded(100);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(r.try_recv(), Err(TryRecvError::Empty));
            thread::sleep(ms(1500));
            assert_eq!(r.try_recv(), Ok(7));
            thread::sleep(ms(500));
            assert_eq!(r.try_recv(), Err(TryRecvError::Closed));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1000));
            s.send(7).unwrap();
        });
    })
    .unwrap();
}

#[test]
fn recv() {
    let (s, r) = bounded(100);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(r.recv(), Ok(7));
            thread::sleep(ms(1000));
            assert_eq!(r.recv(), Ok(8));
            thread::sleep(ms(1000));
            assert_eq!(r.recv(), Ok(9));
            assert_eq!(r.recv(), Err(RecvError));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1500));
            s.send(7).unwrap();
            s.send(8).unwrap();
            s.send(9).unwrap();
        });
    })
    .unwrap();
}

#[test]
fn recv_timeout() {
    let (s, r) = bounded::<i32>(100);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(r.recv_timeout(ms(1000)), Err(RecvTimeoutError::Timeout));
            assert_eq!(r.recv_timeout(ms(1000)), Ok(7));
            assert_eq!(
                r.recv_timeout(ms(1000)),
                Err(RecvTimeoutError::Closed)
            );
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1500));
            s.send(7).unwrap();
        });
    })
    .unwrap();
}

#[test]
fn try_send() {
    let (s, r) = bounded(1);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(s.try_send(1), Ok(()));
            assert_eq!(s.try_send(2), Err(TrySendError::Full(2)));
            thread::sleep(ms(1500));
            assert_eq!(s.try_send(3), Ok(()));
            thread::sleep(ms(500));
            assert_eq!(s.try_send(4), Err(TrySendError::Closed(4)));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1000));
            assert_eq!(r.try_recv(), Ok(1));
            assert_eq!(r.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(r.recv(), Ok(3));
        });
    })
    .unwrap();
}

#[test]
fn send() {
    let (s, r) = bounded(1);

    scope(|scope| {
        scope.spawn(|_| {
            s.send(7).unwrap();
            thread::sleep(ms(1000));
            s.send(8).unwrap();
            thread::sleep(ms(1000));
            s.send(9).unwrap();
            thread::sleep(ms(1000));
            s.send(10).unwrap();
        });
        scope.spawn(|_| {
            thread::sleep(ms(1500));
            assert_eq!(r.recv(), Ok(7));
            assert_eq!(r.recv(), Ok(8));
            assert_eq!(r.recv(), Ok(9));
        });
    })
    .unwrap();
}

#[test]
fn send_timeout() {
    let (s, r) = bounded(2);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(s.send_timeout(1, ms(1000)), Ok(()));
            assert_eq!(s.send_timeout(2, ms(1000)), Ok(()));
            assert_eq!(
                s.send_timeout(3, ms(500)),
                Err(SendTimeoutError::Timeout(3))
            );
            thread::sleep(ms(1000));
            assert_eq!(s.send_timeout(4, ms(1000)), Ok(()));
            thread::sleep(ms(1000));
            assert_eq!(s.send(5), Err(SendError(5)));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1000));
            assert_eq!(r.recv(), Ok(1));
            thread::sleep(ms(1000));
            assert_eq!(r.recv(), Ok(2));
            assert_eq!(r.recv(), Ok(4));
        });
    })
    .unwrap();
}

#[test]
fn send_after_close() {
    let (s, r) = bounded(100);

    s.send(1).unwrap();
    s.send(2).unwrap();
    s.send(3).unwrap();

    drop(r);

    assert_eq!(s.send(4), Err(SendError(4)));
    assert_eq!(s.try_send(5), Err(TrySendError::Closed(5)));
    assert_eq!(
        s.send_timeout(6, ms(500)),
        Err(SendTimeoutError::Closed(6))
    );
}

#[test]
fn recv_after_close() {
    let (s, r) = bounded(100);

    s.send(1).unwrap();
    s.send(2).unwrap();
    s.send(3).unwrap();

    drop(s);

    assert_eq!(r.recv(), Ok(1));
    assert_eq!(r.recv(), Ok(2));
    assert_eq!(r.recv(), Ok(3));
    assert_eq!(r.recv(), Err(RecvError));
}

#[test]
fn close_wakes_sender() {
    let (s, r) = bounded(1);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(s.send(()), Ok(()));
            assert_eq!(s.send(()), Err(SendError(())));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1000));
            drop(r);
        });
    })
    .unwrap();
}

#[test]
fn close_wakes_receiver() {
    let (s, r) = bounded::<()>(1);

    scope(|scope| {
        scope.spawn(move |_| {
            assert_eq!(r.recv(), Err(RecvError));
        });
        scope.spawn(move |_| {
            thread::sleep(ms(1000));
            drop(s);
        });
    })
    .unwrap();
}

#[test]
fn spsc() {
    const COUNT: usize = 100_000;

    let (s, r) = bounded(3);

    scope(|scope| {
        scope.spawn(move |_| {
            for i in 0..COUNT {
                assert_eq!(r.recv(), Ok(i));
            }
            assert_eq!(r.recv(), Err(RecvError));
        });
        scope.spawn(move |_| {
            for i in 0..COUNT {
                s.send(i).unwrap();
            }
        });
    })
    .unwrap();
}

#[test]
fn mpmc() {
    const COUNT: usize = 25_000;
    const THREADS: usize = 4;

    let (s, r) = bounded::<usize>(3);
    let v = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();

    scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for _ in 0..COUNT {
                    let n = r.recv().unwrap();
                    v[n].fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for i in 0..COUNT {
                    s.send(i).unwrap();
                }
            });
        }
    })
    .unwrap();

    for c in v {
        assert_eq!(c.load(Ordering::SeqCst), THREADS);
    }
}

#[test]
fn stress_oneshot() {
    const COUNT: usize = 10_000;

    for _ in 0..COUNT {
        let (s, r) = bounded(1);

        scope(|scope| {
            scope.spawn(|_| r.recv().unwrap());
            scope.spawn(|_| s.send(0).unwrap());
        })
        .unwrap();
    }
}

#[test]
fn stress_iter() {
    const COUNT: usize = 100_000;

    let (request_s, request_r) = bounded(1);
    let (response_s, response_r) = bounded(1);

    scope(|scope| {
        scope.spawn(move |_| {
            let mut count = 0;
            loop {
                for x in response_r.try_iter() {
                    count += x;
                    if count == COUNT {
                        return;
                    }
                }
                request_s.send(()).unwrap();
            }
        });

        for _ in request_r.iter() {
            if response_s.send(1).is_err() {
                break;
            }
        }
    })
    .unwrap();
}

#[test]
fn stress_timeout_two_threads() {
    const COUNT: usize = 100;

    let (s, r) = bounded(2);

    scope(|scope| {
        scope.spawn(|_| {
            for i in 0..COUNT {
                if i % 2 == 0 {
                    thread::sleep(ms(50));
                }
                loop {
                    if let Ok(()) = s.send_timeout(i, ms(10)) {
                        break;
                    }
                }
            }
        });

        scope.spawn(|_| {
            for i in 0..COUNT {
                if i % 2 == 0 {
                    thread::sleep(ms(50));
                }
                loop {
                    if let Ok(x) = r.recv_timeout(ms(10)) {
                        assert_eq!(x, i);
                        break;
                    }
                }
            }
        });
    })
    .unwrap();
}

#[test]
fn drops() {
    const RUNS: usize = 100;

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, PartialEq)]
    struct DropCounter;

    impl Drop for DropCounter {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut rng = thread_rng();

    for _ in 0..RUNS {
        let steps = rng.gen_range(0, 10_000);
        let additional = rng.gen_range(0, 50);

        DROPS.store(0, Ordering::SeqCst);
        let (s, r) = bounded::<DropCounter>(50);

        scope(|scope| {
            scope.spawn(|_| {
                for _ in 0..steps {
                    r.recv().unwrap();
                }
            });

            scope.spawn(|_| {
                for _ in 0..steps {
                    s.send(DropCounter).unwrap();
                }
            });
        })
        .unwrap();

        for _ in 0..additional {
            s.send(DropCounter).unwrap();
        }

        assert_eq!(DROPS.load(Ordering::SeqCst), steps);
        drop(s);
        drop(r);
        assert_eq!(DROPS.load(Ordering::SeqCst), steps + additional);
    }
}

#[test]
fn linearizable() {
    const COUNT: usize = 25_000;
    const THREADS: usize = 4;

    let (s, r) = bounded(THREADS);

    scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for _ in 0..COUNT {
                    s.send(0).unwrap();
                    r.try_recv().unwrap();
                }
            });
        }
    })
    .unwrap();
}

#[test]
fn channel_through_channel() {
    const COUNT: usize = 1000;

    type T = Box<Any + Send>;

    let (s, r) = bounded::<T>(1);

    scope(|scope| {
        scope.spawn(move |_| {
            let mut s = s;

            for _ in 0..COUNT {
                let (new_s, new_r) = bounded(1);
                let mut new_r: T = Box::new(Some(new_r));

                s.send(new_r).unwrap();
                s = new_s;
            }
        });

        scope.spawn(move |_| {
            let mut r = r;

            for _ in 0..COUNT {
                r = r
                    .recv()
                    .unwrap()
                    .downcast_mut::<Option<Receiver<T>>>()
                    .unwrap()
                    .take()
                    .unwrap()
            }
        });
    })
    .unwrap();
}
