use super::*;
use std::sync::Barrier;

#[test]
fn concurrent_terminal_latch_and_send_publish_exactly_once_and_wake_waiters() {
    let termination = Arc::new((Mutex::new(None), Condvar::new()));
    let (tx, rx) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(25));
    let mut workers = Vec::new();
    let (wake_tx, wake_rx) = mpsc::channel();
    for _ in 0..8 {
        let termination = termination.clone();
        let barrier = barrier.clone();
        let wake_tx = wake_tx.clone();
        workers.push(thread::spawn(move || {
            let (state, ready) = &*termination;
            barrier.wait();
            let (guard, timeout) = ready
                .wait_timeout_while(state.lock().unwrap(), Duration::from_secs(2), |v| {
                    v.is_none()
                })
                .unwrap();
            assert!(!timeout.timed_out());
            wake_tx.send(guard.clone()).unwrap();
        }));
    }
    for _ in 0..16 {
        let termination = termination.clone();
        let barrier = barrier.clone();
        let tx = tx.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            publish_child_exit(ChildExit::Exited(Some(7)), &tx, &termination);
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    for _ in 0..8 {
        assert_eq!(
            wake_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(ChildExit::Exited(Some(7)))
        );
    }
    assert_eq!(
        *termination.0.lock().unwrap(),
        Some(ChildExit::Exited(Some(7)))
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Inbound::ChildExited(ChildExit::Exited(Some(7)))
    );
    // A different late outcome cannot replace the latch or send a second event.
    publish_child_exit(ChildExit::Unknown, &tx, &termination);
    assert_eq!(
        *termination.0.lock().unwrap(),
        Some(ChildExit::Exited(Some(7)))
    );
    assert!(rx.try_recv().is_err());
}
